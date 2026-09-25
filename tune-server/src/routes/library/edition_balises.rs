//! « Écrire dans les fichiers » — tranche 4 du chantier « édition des albums,
//! compilations et coffrets » (GO de Bertrand du 25/09/2026).
//!
//! `POST /library/albums/{id}/edition/write-tags`, corps facultatif
//! `{ "dry_run": true }`.
//!
//! Reporte dans les BALISES de chaque fichier local de l'album les valeurs que
//! le mode « Modifier » a posées EN BASE : ALBUM, ALBUMARTIST, DISCNUMBER,
//! DISCTOTAL, DISCSUBTITLE, TRACKNUMBER, TRACKTOTAL, TITLE, ARTIST et
//! COMPILATION (règle : [`edition_album::balises_effectives`]). L'écrivain est
//! celui du dépôt (`metadata::tag_writer`), en mode atomique et gardé :
//! copie, écriture, relecture des champs, empreinte de l'audio, renommage
//! ([`tag_writer::ecrire_balises_edition`]).
//!
//! # Ce qui n'est PAS écrit — `ignores[].raison`
//!
//! - `piste_de_service` : piste d'un service (Qobuz, TIDAL…) — jamais ;
//! - `piste_cue` : piste découpée dans une image par une feuille CUE — les
//!   balises de l'image sont celles de toutes ses pistes (même refus que
//!   `POST /library/write-tags`) ;
//! - `sans_fichier`, `fichier_introuvable` ;
//! - `format_non_gere` : DSF, DFF, WMA… (lofty n'y écrit pas, ou l'audio n'y
//!   est pas isolable pour la preuve) ;
//! - `hors_racines` : le fichier (liens symboliques résolus) n'est sous
//!   aucun dossier de musique déclaré ;
//! - `liens_multiples` : fichier à plusieurs liens physiques — le renommage
//!   atomique le séparerait de ses jumeaux ;
//! - `lecture_seule` / `dossier_en_lecture_seule` ;
//! - `en_lecture` : piste en cours de lecture (ou en pause) sur une zone. On
//!   ne met RIEN en pause : le fichier est sauté et le dit ; relancer
//!   l'écriture après l'arrêt de la zone.
//!
//! # Après l'écriture
//!
//! Chaque fichier écrit est RELU aussitôt, comme le ferait le surveillant
//! (`read_metadata` → ligne piste → tenues de l'édition → taille, date et
//! empreinte d'échantillon à jour) : base et fichiers concordent sans
//! attendre le scan. Les marqueurs `edition_manuelle` / `edition_pistes` sont
//! GARDÉS : voir le module [`edition_album`].
//!
//! # Réponse
//!
//! `{ dry_run, ecrits, a_ecrire, inchanges, plan: [{track_id, path,
//! changements: [{champ, avant, apres}]}], ignores: [{track_id, path,
//! raison}], erreurs: [{track_id, path, message}] }`. En `dry_run`, `ecrits`
//! vaut 0 et `plan` décrit ce qui SERAIT écrit ; sinon `plan` décrit ce qui
//! l'a été.
use std::collections::HashSet;
use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tune_core::db::backend::DbBackend;
use tune_core::db::edition_album::{self, PisteABaliser};
use tune_core::metadata::tag_writer::{self, Changement};

use super::refus;
use crate::state::AppState;

/// Une écriture à la fois, tous albums confondus : deux clics rapprochés ne
/// doivent pas écrire deux fois le même fichier en même temps.
static ECRITURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Default, Deserialize)]
struct Corps {
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlanFichier {
    pub track_id: i64,
    pub path: String,
    pub changements: Vec<Changement>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Ignore {
    pub track_id: i64,
    pub path: String,
    pub raison: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Erreur {
    pub track_id: i64,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct Rapport {
    pub dry_run: bool,
    pub ecrits: usize,
    pub a_ecrire: usize,
    pub inchanges: usize,
    pub plan: Vec<PlanFichier>,
    pub ignores: Vec<Ignore>,
    pub erreurs: Vec<Erreur>,
}

/// Les pistes jouées ou en pause sur une zone, d'après l'état que
/// l'orchestrateur persiste à chaque lecture / pause / arrêt.
fn pistes_en_lecture(db: &std::sync::Arc<dyn DbBackend>) -> HashSet<i64> {
    match db.query_many(
        "SELECT last_track_id FROM zones \
         WHERE last_play_state IN ('playing', 'paused') AND last_track_id IS NOT NULL",
        &[],
    ) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect(),
        Err(e) => {
            tracing::warn!(erreur = %e, "balises_edition_zones_illisibles");
            HashSet::new()
        }
    }
}

/// Les racines de la bibliothèque, liens symboliques résolus.
fn racines(db: &std::sync::Arc<dyn DbBackend>) -> Vec<PathBuf> {
    crate::routes::system::get_music_dirs_list(db)
        .iter()
        .filter_map(|r| std::fs::canonicalize(r.trim()).ok())
        .collect()
}

#[cfg(unix)]
fn dossier_inscriptible(dossier: &FsPath) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dossier.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` est une chaîne C valide, terminée par NUL, qui vit pendant
    // l'appel ; `access` ne la retient pas.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn dossier_inscriptible(dossier: &FsPath) -> bool {
    std::fs::metadata(dossier)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

/// Le fichier réel d'une piste, ou la raison de ne pas y écrire.
fn fichier_ecrivable(
    p: &PisteABaliser,
    racines: &[PathBuf],
    en_lecture: &HashSet<i64>,
) -> Result<PathBuf, &'static str> {
    if p.source != "local" {
        return Err("piste_de_service");
    }
    if p.cue {
        return Err("piste_cue");
    }
    let Some(chemin) = p.chemin.as_deref() else {
        return Err("sans_fichier");
    };
    if !tag_writer::format_balises_edition(chemin) {
        return Err("format_non_gere");
    }
    let sur_disque = tune_core::library::local_path::resolve_existing_local_path(chemin)
        .ok_or("fichier_introuvable")?;
    let reel = std::fs::canonicalize(&sur_disque).map_err(|_| "fichier_introuvable")?;
    if !racines.iter().any(|r| reel.starts_with(r)) {
        return Err("hors_racines");
    }
    let meta = std::fs::metadata(&reel).map_err(|_| "fichier_introuvable")?;
    if !meta.is_file() {
        return Err("fichier_introuvable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err("liens_multiples");
        }
    }
    if meta.permissions().readonly() || std::fs::OpenOptions::new().write(true).open(&reel).is_err()
    {
        return Err("lecture_seule");
    }
    if !reel.parent().is_some_and(dossier_inscriptible) {
        return Err("dossier_en_lecture_seule");
    }
    if en_lecture.contains(&p.id) {
        return Err("en_lecture");
    }
    Ok(reel)
}

/// Le cœur, synchrone (à passer dans `spawn_blocking`). Rend le rapport et
/// les fichiers écrits `(track_id, chemin réel)`.
pub(crate) fn traiter(
    pistes: &[PisteABaliser],
    racines: &[PathBuf],
    en_lecture: &HashSet<i64>,
    dry_run: bool,
) -> (Rapport, Vec<(i64, PathBuf)>) {
    let mut r = Rapport {
        dry_run,
        ..Default::default()
    };
    let mut ecrits = Vec::new();
    for p in pistes {
        let affiche = p.chemin.clone().unwrap_or_default();
        let reel = match fichier_ecrivable(p, racines, en_lecture) {
            Ok(reel) => reel,
            Err(raison) => {
                r.ignores.push(Ignore {
                    track_id: p.id,
                    path: affiche,
                    raison,
                });
                continue;
            }
        };
        let reel_str = reel.to_string_lossy().into_owned();
        let erreur = |message: String| Erreur {
            track_id: p.id,
            path: affiche.clone(),
            message,
        };
        let plan = match tag_writer::plan_balises_edition(&reel_str, &p.balises) {
            Ok(plan) => plan,
            Err(e) => {
                r.erreurs.push(erreur(e));
                continue;
            }
        };
        if plan.is_empty() {
            r.inchanges += 1;
            continue;
        }
        r.a_ecrire += 1;
        if dry_run {
            r.plan.push(PlanFichier {
                track_id: p.id,
                path: affiche,
                changements: plan,
            });
            continue;
        }
        match tag_writer::ecrire_balises_edition(&reel_str, &p.balises) {
            Ok(fait) if fait.is_empty() => r.inchanges += 1,
            Ok(fait) => {
                r.ecrits += 1;
                r.plan.push(PlanFichier {
                    track_id: p.id,
                    path: affiche,
                    changements: fait,
                });
                ecrits.push((p.id, reel));
            }
            Err(e) => {
                tracing::warn!(track_id = p.id, file = %reel_str, erreur = %e, "balises_edition_echec");
                r.erreurs.push(erreur(e));
            }
        }
    }
    (r, ecrits)
}

/// Relit un fichier qu'on vient d'écrire, comme le surveillant le ferait.
fn relire(
    db: &std::sync::Arc<dyn DbBackend>,
    tenues: &edition_album::Tenues,
    track_id: i64,
    reel: &FsPath,
) -> Result<(), String> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(db.clone());
    let mut track = repo
        .get(track_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "piste disparue".to_string())?;
    let m = tune_core::metadata::read_metadata(reel)
        .ok_or_else(|| "relecture des balises impossible".to_string())?;
    super::tracks::apply_metadata_to_track(&mut track, &m);
    tenues.appliquer(&mut track);
    if let Ok(meta) = std::fs::metadata(reel) {
        track.file_size = Some(meta.len() as i64);
        track.file_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as f64);
    }
    track.audio_hash = tune_core::scanner::hasher::compute_audio_hash(reel);
    repo.update(&track).map_err(|e| e.to_string())
}

/// `POST /library/albums/{id}/edition/write-tags`.
pub(super) async fn ecrire_balises(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    corps: Bytes,
) -> Response {
    let corps: Corps = if corps.iter().all(u8::is_ascii_whitespace) {
        Corps::default()
    } else {
        match serde_json::from_slice(&corps) {
            Ok(c) => c,
            Err(e) => {
                return refus(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "corps_invalide",
                    format!("corps attendu : {{ \"dry_run\": bool }} ({e})"),
                );
            }
        }
    };
    // La seconde demande ATTEND la première, puis la voit faite : son plan
    // est vide, elle n'écrit rien deux fois.
    let _verrou = ECRITURE.lock().await;
    let pistes = match edition_album::balises_effectives(&state.backend, id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return refus(
                StatusCode::NOT_FOUND,
                "album_inconnu",
                format!("l'album {id} n'existe pas"),
            );
        }
        Err(e) => {
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erreur_base",
                e.to_string(),
            );
        }
    };
    let racines = racines(&state.backend);
    let en_lecture = pistes_en_lecture(&state.backend);
    let dry_run = corps.dry_run;
    let db = state.backend.clone();
    let travail = tokio::task::spawn_blocking(move || {
        let (mut rapport, ecrits) = traiter(&pistes, &racines, &en_lecture, dry_run);
        if !ecrits.is_empty() {
            let tenues = edition_album::Tenues::charger(&db);
            for (track_id, reel) in &ecrits {
                if let Err(e) = relire(&db, &tenues, *track_id, reel) {
                    tracing::warn!(track_id, erreur = %e, "balises_edition_relecture_echec");
                    rapport.erreurs.push(Erreur {
                        track_id: *track_id,
                        path: reel.to_string_lossy().into_owned(),
                        message: format!("écrit, mais relu sans succès : {e}"),
                    });
                }
            }
        }
        rapport
    })
    .await;
    let rapport = match travail {
        Ok(r) => r,
        Err(e) => {
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ecriture_interrompue",
                e.to_string(),
            );
        }
    };
    if rapport.ecrits > 0 {
        state.event_bus.emit(
            tune_core::event_types::EventType::LibraryUpdated.as_str(),
            json!({ "source": "edition_balises", "album_id": id }),
        );
    }
    tracing::info!(
        album_id = id,
        dry_run,
        ecrits = rapport.ecrits,
        a_ecrire = rapport.a_ecrire,
        ignores = rapport.ignores.len(),
        erreurs = rapport.erreurs.len(),
        "balises_edition"
    );
    Json(rapport).into_response()
}

#[cfg(all(test, unix))]
#[path = "edition_balises_tests.rs"]
mod tests;
