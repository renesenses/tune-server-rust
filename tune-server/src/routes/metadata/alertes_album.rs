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
//!   `{"album_id": 12, "reason": "very_short", "track_ids": [2, 3, 4]}` —
//!   marque l'alerte normale pour CES pistes : celles que l'aperçu a montrées.
//!   Le serveur VÉRIFIE que chacune est de l'album et porte l'alerte (400
//!   sinon, rien n'est écrit), puis écrit UNE LIGNE PAR PISTE, dans une
//!   transaction. 🔴 Jamais de ligne de portée `album` pour ce geste : sa clé
//!   (dossier commun + titre) couvrirait aussi une piste ajoutée plus tard,
//!   un homonyme du même dossier (compilation à plat, image CUE sans TITLE),
//!   tout un dossier très large après une fusion — or le testeur n'a accepté
//!   le geste QUE pour les pistes affichées. La réponse rend les clés des
//!   lignes réellement AJOUTÉES (`added_keys`) : ce sont elles, et elles
//!   seules, que l'annulation retire. Rejoué : `added: 0`, `added_keys: []`.
//! * `DELETE /metadata/doubtful/ignored/album` `{"keys": [{scope, key, title, reason}, …]}`
//!   — annule le geste : retire exactement ces lignes (celles que le POST a
//!   ajoutées), dans une transaction. Un choix fait piste par piste avant le
//!   geste n'en fait pas partie et reste. Rejoué : `removed: 0`.
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
    // Une lecture en échec ne se maquille pas en « rien d'ignoré ».
    let deja = match ignorees::lister(&state.backend) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_lecture");
            return erreur(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    };
    let (couvertes, a_marquer): (Vec<&Track>, Vec<&Track>) = signalees(&pistes, &p.reason)
        .into_iter()
        .partition(|t| deja.iter().any(|a| a.couvre(t, &p.reason)));
    Json(json!({
        "album_id": p.album_id,
        "album_title": pistes[0].album_title,
        "reason": p.reason,
        // Le geste groupé n'écrit que des lignes de pistes.
        "scope": Portee::Piste.as_str(),
        // Affichage seulement : où sont les pistes de l'album.
        "folder": ignorees::dossier_commun(pistes.iter().filter_map(|t| {
            t.file_path.as_deref().filter(|c| !c.is_empty()).or(t.cue_media_path.as_deref())
        })),
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
    track_ids: Vec<i64>,
}

fn cle_json(a: &AlerteIgnoree) -> Value {
    json!({"scope": a.portee.as_str(), "key": a.cle, "title": a.titre, "reason": a.raison})
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
    let toutes: BTreeSet<i64> = concernees.iter().filter_map(|t| t.id).collect();
    let refusees: Vec<i64> = d
        .track_ids
        .iter()
        .copied()
        .filter(|id| !toutes.contains(id))
        .collect();
    if d.track_ids.is_empty() || !refusees.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "track_ids must be tracks of this album flagged with this reason",
                "invalid_track_ids": refusees,
            })),
        )
            .into_response();
    }
    let ids: BTreeSet<i64> = d.track_ids.iter().copied().collect();
    let choisies: Vec<&Track> = concernees
        .into_iter()
        .filter(|t| t.id.is_some_and(|id| ids.contains(&id)))
        .collect();
    let maintenant = maintenant();
    let lignes: Vec<AlerteIgnoree> = choisies
        .iter()
        .filter_map(|t| ignorees::alerte_de_la_piste(t, &d.reason, maintenant))
        .collect();
    if lignes.len() != choisies.len() {
        return erreur(
            StatusCode::BAD_REQUEST,
            "a track has no stable key (no path)",
        );
    }
    let ajoutees = match ignorees::ignorer_lot(&state.backend, &lignes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_album_ecriture");
            return erreur(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    };
    let added_keys: Vec<Value> = lignes
        .iter()
        .zip(&ajoutees)
        .filter(|(_, ajoutee)| **ajoutee)
        .map(|(a, _)| cle_json(a))
        .collect();
    Json(json!({
        "status": "ok",
        "album_id": d.album_id,
        "reason": d.reason,
        "scope": Portee::Piste.as_str(),
        // Les pistes désormais couvertes (ajoutées, ou déjà normales).
        "track_ids": choisies.iter().filter_map(|t| t.id).collect::<Vec<_>>(),
        "track_count": choisies.len(),
        // Les lignes que CE geste a ajoutées : ce que l'annulation retire.
        "added": added_keys.len(),
        "added_keys": added_keys,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct CleAnnulee {
    scope: String,
    key: String,
    #[serde(default)]
    title: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AnnulationAlbum {
    keys: Vec<CleAnnulee>,
}

pub(super) async fn retablir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<AnnulationAlbum>,
) -> impl IntoResponse {
    let mut lot = Vec::with_capacity(d.keys.len());
    for k in d.keys {
        let Some(portee) = Portee::depuis(&k.scope) else {
            return erreur(
                StatusCode::BAD_REQUEST,
                format!("unknown scope: {}", k.scope),
            );
        };
        lot.push((portee, k.key, k.title, k.reason));
    }
    match ignorees::retablir_lot(&state.backend, &lot) {
        Ok(n) => Json(json!({"status": "ok", "removed": n})).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_album_ecriture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}
