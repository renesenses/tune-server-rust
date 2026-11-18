//! #9028 — « Normal pour tout l'album » : un seul geste pour toutes les
//! alertes d'un même type d'un album (suite de #8958, go de Bertrand le
//! 18/11 pour la rc13, forum fil 2685).
//!
//! Cas réel : un album en un seul fichier + feuille CUE dont huit pistes
//! cachées de 3 à 9 s déclenchent chacune « Piste de moins de 5 secondes ».
//! Le testeur accepte le geste groupé SI l'écran montre que toutes ces
//! pistes viennent du même album : d'où l'aperçu, qui les liste.
//!
//! * `GET /metadata/doubtful/album?album_id=12&reason=very_short` — l'aperçu :
//!   l'album (titre, dossier), les pistes qui portent encore l'alerte, et la
//!   portée qui sera retenue. Lecture seule.
//! * `POST /metadata/doubtful/ignored/album`
//!   `{"album_id": 12, "reason": "very_short", "track_ids": [..]}` — marque
//!   l'alerte normale. `track_ids` (facultatif) : les pistes que l'écran a
//!   montrées ; le serveur VÉRIFIE que chacune est de l'album et porte
//!   l'alerte (400 sinon, rien n'est écrit).
//!   - toutes les pistes de l'album qui portent l'alerte (ou pas de
//!     `track_ids`) : UNE ligne de portée `album` (dossier + titre, la clé
//!     stable de #8958, que le filtre SQL sait déjà lire) ;
//!   - une partie seulement, ou un album sans dossier exploitable : une ligne
//!     par piste — le geste ne couvre jamais plus que ce que l'écran a montré.
//!
//!   Rejoué, il ne change rien (`added: 0`).
//! * `DELETE /metadata/doubtful/ignored/album` `{"album_id": 12, "reason": "very_short"}`
//!   — « ne plus considérer comme normal » pour l'album : retire TOUS les
//!   choix de cette raison qui couvrent une piste de l'album (ligne d'album,
//!   lignes de pistes posées une à une ou par le repli ci-dessus). Rejoué :
//!   `removed: 0`, pas d'erreur.
//!
//! Les écritures sont réservées à l'administrateur, comme le reste de #8958.
//! Aucune table nouvelle : `alertes_douteuses_ignorees`, SQLite et
//! PostgreSQL, requêtes communes aux deux moteurs.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tune_core::db::alertes_douteuses_ignorees::{self as ignorees, AlerteIgnoree, Portee};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

fn erreur(code: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({"error": message.into()}))).into_response()
}

fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// La raison est-elle connue, et a-t-elle un sens pour un album ?
fn raison_valide(raison: &str) -> Result<(), axum::response::Response> {
    if !ignorees::RAISONS.contains(&raison) {
        return Err(erreur(
            StatusCode::BAD_REQUEST,
            format!("unknown reason: {raison}"),
        ));
    }
    if raison == "missing_album" {
        // Une piste sans album n'est d'aucun album.
        return Err(erreur(
            StatusCode::BAD_REQUEST,
            "missing_album has no album scope",
        ));
    }
    Ok(())
}

/// Les pistes LOCALES de l'album (celles que `/metadata/doubtful` peut
/// lister). 404 quand l'album n'en a aucune.
fn pistes_de_l_album(
    state: &AppState,
    album_id: i64,
) -> Result<Vec<Track>, axum::response::Response> {
    let pistes: Vec<Track> = TrackRepo::with_backend(state.backend.clone())
        .list_by_album(album_id)
        .map_err(|e| erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .filter(|t| t.source.is_empty() || t.source == "local")
        .collect();
    if pistes.is_empty() {
        return Err(erreur(StatusCode::NOT_FOUND, "album not found"));
    }
    Ok(pistes)
}

/// Les pistes de l'album qui portent l'alerte `raison`, ignorée ou non.
fn signalees<'a>(pistes: &'a [Track], raison: &str) -> Vec<&'a Track> {
    pistes
        .iter()
        .filter(|t| ignorees::raisons_de(t).contains(&raison))
        .collect()
}

fn decrire_piste(t: &Track) -> Value {
    json!({
        "id": t.id,
        "title": t.title,
        "duration_ms": t.duration_ms,
        "disc_number": t.disc_number,
        "track_number": t.track_number,
        "file_path": t.file_path.as_deref().filter(|p| !p.is_empty()).or(t.cue_media_path.as_deref()),
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct ApercuParams {
    album_id: i64,
    reason: String,
}

pub(super) async fn apercu(
    State(state): State<AppState>,
    Query(p): Query<ApercuParams>,
) -> impl IntoResponse {
    if let Err(r) = raison_valide(&p.reason) {
        return r;
    }
    let pistes = match pistes_de_l_album(&state, p.album_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let deja = ignorees::lister(&state.backend).unwrap_or_default();
    let (couvertes, a_marquer): (Vec<&Track>, Vec<&Track>) = signalees(&pistes, &p.reason)
        .into_iter()
        .partition(|t| deja.iter().any(|a| a.couvre(t, &p.reason)));
    let album = ignorees::alerte_de_l_album(&pistes, &p.reason, 0);
    Json(json!({
        "album_id": p.album_id,
        "album_title": pistes[0].album_title,
        "reason": p.reason,
        // Où le choix sera mémorisé pour l'album entier : `album` (dossier +
        // titre), ou `track` (une ligne par piste) faute de dossier.
        "scope": album.as_ref().map(|a| a.portee).unwrap_or(Portee::Piste).as_str(),
        "folder": album.as_ref().map(|a| a.cle.clone()),
        "tracks": a_marquer.iter().map(|t| decrire_piste(t)).collect::<Vec<_>>(),
        "total": a_marquer.len(),
        "ignored_count": couvertes.len(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct DemandeAlbum {
    album_id: i64,
    reason: String,
    #[serde(default)]
    track_ids: Option<Vec<i64>>,
}

pub(super) async fn ignorer(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<DemandeAlbum>,
) -> impl IntoResponse {
    if let Err(r) = raison_valide(&d.reason) {
        return r;
    }
    let pistes = match pistes_de_l_album(&state, d.album_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let concernees = signalees(&pistes, &d.reason);
    if concernees.is_empty() {
        return erreur(
            StatusCode::BAD_REQUEST,
            format!("reason does not apply to this album: {}", d.reason),
        );
    }
    let toutes: BTreeSet<i64> = concernees.iter().filter_map(|t| t.id).collect();
    let choisies: Vec<&Track> = match &d.track_ids {
        None => concernees.clone(),
        Some(ids) => {
            let refusees: Vec<i64> = ids
                .iter()
                .copied()
                .filter(|id| !toutes.contains(id))
                .collect();
            if !refusees.is_empty() || ids.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "track_ids must be tracks of this album flagged with this reason",
                        "invalid_track_ids": refusees,
                    })),
                )
                    .into_response();
            }
            let ids: BTreeSet<i64> = ids.iter().copied().collect();
            concernees
                .iter()
                .copied()
                .filter(|t| t.id.is_some_and(|id| ids.contains(&id)))
                .collect()
        }
    };
    let maintenant = maintenant();
    let tout_l_album = choisies.len() == concernees.len();
    let lignes: Vec<AlerteIgnoree> = match tout_l_album
        .then(|| ignorees::alerte_de_l_album(&pistes, &d.reason, maintenant))
        .flatten()
    {
        Some(a) => vec![a],
        None => choisies
            .iter()
            .filter_map(|t| ignorees::alerte_de_la_piste(t, &d.reason, maintenant))
            .collect(),
    };
    if lignes.is_empty() {
        return erreur(
            StatusCode::BAD_REQUEST,
            "tracks have no stable key (no path)",
        );
    }
    let mut ajoutees = 0usize;
    for a in &lignes {
        match ignorees::ignorer(&state.backend, a) {
            Ok(true) => ajoutees += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, "alertes_ignorees_album_ecriture");
                return erreur(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
        }
    }
    let portee = lignes[0].portee;
    Json(json!({
        "status": "ok",
        "album_id": d.album_id,
        "reason": d.reason,
        "scope": portee.as_str(),
        // La clé de la ligne d'album, pour l'annulation fine (`DELETE
        // /doubtful/ignored`) ; vide pour des lignes de pistes.
        "key": if portee == Portee::Album { lignes[0].cle.clone() } else { String::new() },
        "title": lignes[0].titre,
        "track_ids": choisies.iter().filter_map(|t| t.id).collect::<Vec<_>>(),
        "track_count": choisies.len(),
        "added": ajoutees,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct AnnulationAlbum {
    album_id: i64,
    reason: String,
}

pub(super) async fn retablir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<AnnulationAlbum>,
) -> impl IntoResponse {
    if let Err(r) = raison_valide(&d.reason) {
        return r;
    }
    let pistes = match pistes_de_l_album(&state, d.album_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let deja = match ignorees::lister(&state.backend) {
        Ok(v) => v,
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Tout choix de cette raison qui couvre une piste de l'album, qu'il
    // porte sur l'album ou sur une piste.
    let mut retirees = 0usize;
    for a in deja
        .iter()
        .filter(|a| pistes.iter().any(|t| a.couvre(t, &d.reason)))
    {
        match ignorees::retablir(&state.backend, a.portee, &a.cle, &a.titre, &a.raison) {
            Ok(true) => retirees += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, "alertes_ignorees_album_ecriture");
                return erreur(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
        }
    }
    Json(json!({"status": "ok", "album_id": d.album_id, "reason": d.reason, "removed": retirees}))
        .into_response()
}
