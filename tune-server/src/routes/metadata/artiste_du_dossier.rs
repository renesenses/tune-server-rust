//! #8679 — « Albums douteux » : écrire l'artiste que donne le DOSSIER quand la
//! balise d'artiste manque.
//!
//! Go de Bertrand du 13/11/2026 (recette rc12). Une piste sans balise
//! d'artiste rangée sous `…/Pink Floyd/Animals/01.flac` était signalée
//! « artiste manquant » alors que le scan sait lire l'artiste dans le chemin.
//! La règle est CELLE DU SCAN ([`tune_core::metadata::artiste_du_dossier`]) :
//! dossier de disque sauté, dossiers génériques, volumes et points de montage
//! écartés. L'écran ne devine plus rien de son côté.
//!
//! `POST /metadata/doubtful/artist-from-folder`, réservé à l'administrateur :
//!
//! * `{"dry_run": true}` — l'APERÇU : combien de pistes, pour quels artistes,
//!   rien n'est écrit ;
//! * `{"track_ids": [..]}` — ces pistes seulement (le bouton d'une ligne) ;
//!   sans `track_ids`, toute la liste des douteuses.
//!
//! L'écriture :
//!
//! 1. **en base** : l'artiste de la piste, rattaché à l'artiste existant de ce
//!    nom (sans tenir compte de la casse), sinon créé — le chemin de
//!    `batch_set_artist` ([`super::artiste_par_nom`]). Le champ est TENU : une
//!    analyse complète ne le défait pas. L'album prend cet artiste s'il n'en
//!    avait pas ET que toutes ses pistes ont désormais ce même artiste — un
//!    album « Ultimate » d'artiste inconnu partagé par deux dossiers
//!    d'artistes différents n'est pas attribué au premier venu ;
//! 2. **dans le fichier** (balise ARTIST seule), SEULEMENT si le réglage
//!    « Écrire les modifications dans les fichiers audio » est coché
//!    ([`crate::routes::ecriture_fichiers::autorisee`]), relu avant chaque
//!    fichier, par l'écrivain atomique de « Graver tags »
//!    ([`write_tags_atomique`]). Réglage décoché : aucun fichier n'est ouvert.
//!
//! Une piste qui a déjà un artiste, qui n'est pas locale, ou dont le dossier
//! ne dit rien, est écartée : la route ne fait QUE ce que l'aperçu annonce.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::tag_writer::{TagUpdate, write_tags_atomique};

use crate::state::AppState;

/// Le nom que la liste des douteuses tient pour « pas d'artiste ».
const ARTISTE_INCONNU: &str = "Unknown Artist";

/// Pistes lues par page quand toute la liste est demandée.
const PAGE: i64 = 1000;

/// La piste n'a pas d'artiste — même prédicat que la raison `missing_artist`
/// de `GET /metadata/doubtful`.
pub(crate) fn artiste_absent(t: &Track) -> bool {
    t.artist_name
        .as_deref()
        .map(|a| a.is_empty() || a == ARTISTE_INCONNU)
        .unwrap_or(true)
}

/// Une piste de la bibliothèque LOCALE.
fn locale(t: &Track) -> bool {
    t.source.is_empty() || t.source == "local"
}

/// L'artiste du dossier d'une piste douteuse pour cette raison, s'il y en a un.
pub(crate) fn artiste_du_dossier_de(t: &Track) -> Option<String> {
    if !artiste_absent(t) || !locale(t) {
        return None;
    }
    tune_core::metadata::artiste_du_dossier(t.file_path.as_deref()?)
}

/// Une piste que la route réparera.
#[derive(Debug, Clone)]
pub(crate) struct Candidat {
    pub track_id: i64,
    pub file_path: String,
    pub album_id: Option<i64>,
    pub artiste: String,
}

fn candidat(t: &Track) -> Option<Candidat> {
    Some(Candidat {
        track_id: t.id?,
        file_path: t.file_path.clone()?,
        album_id: t.album_id,
        artiste: artiste_du_dossier_de(t)?,
    })
}

/// Les candidates : les pistes nommées, ou toute la liste des douteuses.
pub(crate) fn candidats(
    backend: &Arc<dyn DbBackend>,
    ids: Option<&[i64]>,
) -> Result<Vec<Candidat>, String> {
    let repo = TrackRepo::with_backend(backend.clone());
    let pistes = match ids {
        Some(ids) => {
            let uniques: Vec<i64> = ids
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            repo.get_multiple(&uniques).map_err(|e| e.to_string())?
        }
        None => {
            // Lue en entier AVANT d'écrire : écrire pendant la pagination
            // ferait sauter des pistes (l'offset glisse).
            let mut toutes = Vec::new();
            let mut offset = 0;
            loop {
                let page = repo
                    .list_doubtful(PAGE, offset)
                    .map_err(|e| e.to_string())?;
                let n = page.len() as i64;
                toutes.extend(page);
                if n < PAGE {
                    break;
                }
                offset += n;
            }
            toutes
        }
    };
    Ok(pistes.iter().filter_map(candidat).collect())
}

/// Ce que l'écriture en base a fait.
#[derive(Debug, Default)]
pub(crate) struct Bilan {
    pub pistes: i64,
    pub echecs: i64,
    pub artistes_crees: i64,
    pub artistes_existants: i64,
    pub albums: i64,
    /// `(piste, chemin, nom de l'artiste tel qu'enregistré)` des pistes écrites.
    pub ecrites: Vec<(i64, String, String)>,
}

/// L'album n'a pas d'artiste (aucun, ou l'artiste inconnu).
fn album_sans_artiste(backend: &Arc<dyn DbBackend>, album_id: i64) -> bool {
    let params: [&dyn ToSqlValue; 1] = [&album_id];
    match backend.query_one(
        "SELECT al.artist_id, ar.name FROM albums al \
         LEFT JOIN artists ar ON ar.id = al.artist_id WHERE al.id = ?1",
        &params,
    ) {
        Ok(Some(l)) => {
            let id = l.first().and_then(|v| v.as_i64());
            let nom = l.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            id.is_none() || nom.is_empty() || nom == ARTISTE_INCONNU
        }
        _ => false,
    }
}

/// Toutes les pistes de l'album ont cet artiste.
fn album_d_un_seul_artiste(backend: &Arc<dyn DbBackend>, album_id: i64, artist_id: i64) -> bool {
    let params: [&dyn ToSqlValue; 2] = [&album_id, &artist_id];
    backend
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?1 \
             AND (artist_id IS NULL OR artist_id <> ?2)",
            &params,
        )
        .ok()
        .flatten()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        == Some(0)
}

/// Écrit en BASE l'artiste du dossier de chaque candidate.
pub(crate) fn ecrire_en_base(backend: &Arc<dyn DbBackend>, cands: &[Candidat]) -> Bilan {
    let mut bilan = Bilan::default();
    // Un nom, une résolution : « prince » et « Prince » tombent sur le même.
    let mut resolus: HashMap<String, Option<(i64, String)>> = HashMap::new();
    let mut albums: BTreeMap<i64, i64> = BTreeMap::new();
    for c in cands {
        let cle = c.artiste.to_lowercase();
        let resolu = resolus
            .entry(cle)
            .or_insert_with(|| match super::artiste_par_nom(backend, &c.artiste) {
                Ok((a, id, cree)) => {
                    if cree {
                        bilan.artistes_crees += 1;
                    } else {
                        bilan.artistes_existants += 1;
                    }
                    Some((id, a.name))
                }
                Err(e) => {
                    tracing::warn!(artiste = %c.artiste, error = %e, "artiste_du_dossier_resolution");
                    None
                }
            })
            .clone();
        let Some((artist_id, nom)) = resolu else {
            bilan.echecs += 1;
            continue;
        };
        let params: [&dyn ToSqlValue; 2] = [&artist_id, &c.track_id];
        match backend.execute("UPDATE tracks SET artist_id = ?1 WHERE id = ?2", &params) {
            Ok(n) if n > 0 => {
                tune_core::db::champs_tenus::tenir_par_id(
                    backend,
                    c.track_id,
                    &[tune_core::db::champs_tenus::Champ::Artiste],
                );
                bilan.pistes += 1;
                bilan.ecrites.push((c.track_id, c.file_path.clone(), nom));
                if let Some(al) = c.album_id {
                    albums.insert(al, artist_id);
                }
            }
            Ok(_) => bilan.echecs += 1,
            Err(e) => {
                tracing::warn!(track_id = c.track_id, error = %e, "artiste_du_dossier_piste");
                bilan.echecs += 1;
            }
        }
    }
    for (album_id, artist_id) in albums {
        if !album_sans_artiste(backend, album_id)
            || !album_d_un_seul_artiste(backend, album_id, artist_id)
        {
            continue;
        }
        let params: [&dyn ToSqlValue; 2] = [&artist_id, &album_id];
        match backend.execute("UPDATE albums SET artist_id = ?1 WHERE id = ?2", &params) {
            Ok(n) if n > 0 => bilan.albums += 1,
            Ok(_) => {}
            Err(e) => tracing::warn!(album_id, error = %e, "artiste_du_dossier_album"),
        }
    }
    bilan
}

/// Corps de `POST /metadata/doubtful/artist-from-folder`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Demande {
    /// Ces pistes ; absent : toute la liste des douteuses.
    #[serde(default)]
    track_ids: Option<Vec<i64>>,
    /// Vrai : l'aperçu seulement, rien n'est écrit.
    #[serde(default)]
    dry_run: bool,
}

pub(crate) async fn ecrire_artiste_du_dossier(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<Demande>,
) -> impl IntoResponse {
    if d.track_ids.as_ref().is_some_and(|v| v.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "track_ids cannot be empty"})),
        )
            .into_response();
    }
    let cands = match candidats(&state.backend, d.track_ids.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "artiste_du_dossier_lecture");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };
    let ecriture = crate::routes::ecriture_fichiers::autorisee(&state);

    if d.dry_run {
        let mut par_nom: BTreeMap<String, i64> = BTreeMap::new();
        for c in &cands {
            *par_nom.entry(c.artiste.clone()).or_default() += 1;
        }
        let mut artistes: Vec<(String, i64)> = par_nom.into_iter().collect();
        artistes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        return Json(json!({
            "tracks": cands.len(),
            "artists": artistes
                .iter()
                .map(|(n, k)| json!({"name": n, "tracks": k}))
                .collect::<Vec<_>>(),
            crate::routes::ecriture_fichiers::CHAMP_REPONSE: ecriture,
        }))
        .into_response();
    }

    let bilan = ecrire_en_base(&state.backend, &cands);
    let fichiers = if ecriture { bilan.ecrites.len() } else { 0 };
    if ecriture && !bilan.ecrites.is_empty() {
        let state2 = state.clone();
        let ecrites = bilan.ecrites.clone();
        tokio::spawn(async move { ecrire_les_fichiers(state2, ecrites).await });
    }
    tracing::info!(
        pistes = bilan.pistes,
        echecs = bilan.echecs,
        artistes_crees = bilan.artistes_crees,
        albums = bilan.albums,
        fichiers,
        "artiste_du_dossier_ecrit"
    );
    Json(json!({
        "updated": bilan.pistes,
        "errors": bilan.echecs,
        "artists_created": bilan.artistes_crees,
        "artists_linked": bilan.artistes_existants,
        "albums_updated": bilan.albums,
        crate::routes::ecriture_fichiers::CHAMP_REPONSE: ecriture,
        "files_scheduled": fichiers,
    }))
    .into_response()
}

/// La balise ARTIST de chaque fichier, en tâche de fond : une liste de
/// 1 500 pistes copierait 1 500 fichiers, la requête n'attend pas.
async fn ecrire_les_fichiers(state: AppState, ecrites: Vec<(i64, String, String)>) {
    let pistes = TrackRepo::with_backend(state.backend.clone());
    let (mut ecrits, mut sautes, mut erreurs) = (0usize, 0usize, 0usize);
    for (track_id, chemin, nom) in ecrites {
        // Relu avant CHAQUE fichier : décoché en cours de route, on s'arrête.
        if !crate::routes::ecriture_fichiers::autorisee(&state) {
            tracing::warn!(track_id, "artiste_du_dossier_reglage_coupe_arret");
            break;
        }
        if tune_core::library::local_path::resolve_local_path(&chemin).is_missing()
            || crate::routes::library::graver_dr::fichier_en_lecture(
                &state.playback,
                &pistes,
                &chemin,
            )
            .await
        {
            sautes += 1;
            continue;
        }
        let maj = TagUpdate {
            artist_name: Some(nom),
            ..Default::default()
        };
        match write_tags_atomique(&chemin, &maj).await {
            Ok(r) => {
                ecrits += 1;
                if r.fields_written > 0 {
                    crate::routes::library::write_tags::remettre_la_ligne_en_phase(
                        &state.backend,
                        &chemin,
                    );
                }
            }
            Err(e) => {
                tracing::warn!(track_id, file = %chemin, error = %e, "artiste_du_dossier_fichier_en_echec");
                erreurs += 1;
            }
        }
    }
    tracing::info!(ecrits, sautes, erreurs, "artiste_du_dossier_fichiers_fin");
}

#[cfg(test)]
#[path = "artiste_du_dossier_tests_8679.rs"]
mod tests_8679;
