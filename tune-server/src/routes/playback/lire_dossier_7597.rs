//! #7597 — lire, ou ajouter à la file, un dossier avec TOUT son sous-arbre,
//! dans l'ordre (fil forum 2577, « Adding folder structure recursively to
//! queue? »).
//!
//! `POST /zones/{id}/play` et `POST /zones/{id}/queue/add` acceptent `folder`
//! (un chemin absolu de la vue Répertoires), à côté d'`album_id` et de
//! `track_ids`. Les trois gestes d'un album valent donc pour un dossier :
//! lire maintenant (`play`), lire ensuite (`queue/add` avec `position`),
//! ajouter à la fin (`queue/add` sans `position`).
//!
//! La liste est construite ICI, côté serveur : le client n'a que la page qu'il
//! affiche (les pistes du dossier ouvert, ou une liste tronquée à son
//! `LOAD_LIMIT` dans Oxygène), jamais le sous-arbre entier.
//!
//! Quatre refus, chacun nommé, et aucune lecture partielle :
//! * `folder_invalid` — chemin relatif, ou qui contient `.`/`..` ;
//! * `folder_outside_library` — chemin hors des dossiers musicaux ;
//! * `folder_too_large` — plus de [`PLAFOND_DOSSIER`] pistes : on refuse en
//!   disant combien, plutôt que d'en lire une partie en silence ;
//! * `folder_no_tracks` — rien de jouable dans le sous-arbre.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tune_core::db::track_repo::TrackRepo;
use unicode_normalization::UnicodeNormalization as _;

use crate::state::AppState;

/// Le plus grand dossier qu'on lit d'un geste. Au-delà, la file elle-même
/// devient l'ennemi (#2228 : 30 000 lignes gelaient l'interface) ; la lecture
/// aléatoire en reste à quelques centaines pour cette raison.
pub(super) const PLAFOND_DOSSIER: i64 = 5_000;

/// Pourquoi un dossier n'est pas lu.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefusDossier {
    Invalide,
    HorsBibliotheque,
    TropGrand { total: i64 },
    Vide,
    Base(String),
}

/// Les racines où un dossier peut se trouver : les dossiers musicaux
/// configurés (réglage `music_dirs`, sinon la configuration — la même source
/// que `GET /library/browse`), plus la racine que la vue Répertoires déduit des
/// données quand aucune racine configurée n'a de piste (même repli).
fn racines(state: &AppState) -> Vec<String> {
    let mut dirs: Vec<String> =
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("music_dirs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| state.config.music_dirs.clone());
    if let Some(base) = tune_core::db::track_repo::derive_common_root(state.backend.as_ref()) {
        dirs.push(base);
    }
    dirs
}

/// Le dossier demandé, validé et replié en NFC, ou le refus.
///
/// Le contrôle est LEXICAL : la base, pas le disque, dit ce qu'un dossier
/// contient, et un partage décroché ne doit pas empêcher de lire ce qui est
/// scanné. Il refuse donc tout composant `.` ou `..` — `Path::starts_with`
/// compare composant par composant et prendrait `/musique/../etc` pour un
/// chemin sous `/musique`.
pub(super) fn dossier_valide(state: &AppState, brut: &str) -> Result<String, RefusDossier> {
    use std::path::Path;
    let norme: String = tune_core::scanner::walker::normalize_path(brut)
        .nfc()
        .collect();
    let chemin = Path::new(&norme);
    // Sur le TEXTE et non sur `Path::components` : celui-ci efface en silence
    // un `.` intermédiaire (`/a/./b` → `a`, `b`), et le motif `LIKE`, lui, le
    // garderait tel quel.
    if norme.is_empty()
        || !chemin.is_absolute()
        || norme.split(['/', '\\']).any(|s| s == "." || s == "..")
    {
        return Err(RefusDossier::Invalide);
    }
    let dedans = racines(state).iter().any(|r| {
        let racine: String = tune_core::scanner::walker::normalize_path(r)
            .nfc()
            .collect();
        !racine.is_empty() && chemin.starts_with(&racine)
    });
    if !dedans {
        return Err(RefusDossier::HorsBibliotheque);
    }
    Ok(norme)
}

/// Les pistes de `brut` et de ses sous-dossiers, dans l'ordre de lecture
/// (`TrackRepo::pistes_du_dossier_dans_l_ordre`), pour le profil qui agit.
pub(super) fn pistes_du_dossier(
    state: &AppState,
    brut: &str,
    profile_id: i64,
) -> Result<Vec<i64>, RefusDossier> {
    let dossier = dossier_valide(state, brut)?;
    let (ids, total) = TrackRepo::with_backend(state.backend.clone())
        .pistes_du_dossier_dans_l_ordre(profile_id, &dossier, PLAFOND_DOSSIER)
        .map_err(|e| RefusDossier::Base(e.to_string()))?;
    if total > PLAFOND_DOSSIER {
        tracing::warn!(
            dossier,
            total,
            plafond = PLAFOND_DOSSIER,
            "dossier_trop_grand"
        );
        return Err(RefusDossier::TropGrand { total });
    }
    if ids.is_empty() {
        return Err(RefusDossier::Vide);
    }
    tracing::info!(dossier, pistes = ids.len(), "dossier_resolu");
    Ok(ids)
}

/// La réponse d'un refus, nommée et traduite (`i18n_server.json`).
pub(super) fn reponse(refus: RefusDossier, zone_id: i64) -> Response {
    use crate::message_api::refus as r;
    match refus {
        RefusDossier::Invalide => r(
            StatusCode::BAD_REQUEST,
            "folder_invalid",
            "api.playback.folderInvalid",
            &[],
        ),
        RefusDossier::HorsBibliotheque => r(
            StatusCode::BAD_REQUEST,
            "folder_outside_library",
            "api.playback.folderOutsideLibrary",
            &[],
        ),
        RefusDossier::TropGrand { total } => r(
            StatusCode::BAD_REQUEST,
            "folder_too_large",
            "api.playback.folderTooLarge",
            &[
                ("count", total.to_string()),
                ("max", PLAFOND_DOSSIER.to_string()),
            ],
        ),
        RefusDossier::Vide => r(
            StatusCode::BAD_REQUEST,
            "folder_no_tracks",
            "api.playback.folderNoTracks",
            &[],
        ),
        RefusDossier::Base(e) => super::lecture_base_echouee("pistes_du_dossier", zone_id, e),
    }
    .into_response()
}
