//! #8452 — Cover+ par station : `GET` et `PUT /radios/{id}/cover-plus`.
//!
//! Cover+ cherche la pochette du titre en cours d'une webradio qui n'en
//! publie pas (`tune_core::cover_plus`). Il est actif par défaut ; ce réglage
//! le COUPE pour une station de « Mes radios » — une station dont l'ICY est
//! trompeur, ou dont on préfère voir le logo.
//!
//! Corps : `{"enabled": bool}`, dans les deux sens.
//!
//! Et l'interrupteur du serveur, `GET` et `PUT /radios/cover-plus` : Cover+
//! est livré ÉTEINT (`enabled: false`) ; `artist_photo` coupe le seul repli
//! « photo de l'artiste », qui lit une page web d'Apple et non une API. Corps
//! `{"enabled": bool?, "artist_photo": bool?}` ; la réponse rend les deux.
//! Une erreur de base rend un 500 sans texte (journalisé côté serveur) : il
//! n'y a aucun message à traduire.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tune_core::db::radio_repo::RadioRepo;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct CorpsCoverPlus {
    enabled: bool,
}

fn station_connue(state: &AppState, id: i64) -> bool {
    RadioRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .is_some()
}

async fn lire(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if !station_connue(&state, id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let actif = tune_core::cover_plus::actif_pour_la_station(&state.backend, Some(id));
    Json(json!({ "id": id, "enabled": actif })).into_response()
}

async fn regler(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<CorpsCoverPlus>,
) -> Response {
    if !station_connue(&state, id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tune_core::cover_plus::regler_la_station(&state.backend, id, corps.enabled) {
        Ok(()) => Json(json!({ "id": id, "enabled": corps.enabled })).into_response(),
        Err(e) => {
            tracing::warn!(station = id, erreur = %e, "cover_plus_reglage_station_echec");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct CorpsInterrupteur {
    enabled: Option<bool>,
    artist_photo: Option<bool>,
}

fn etat_du_serveur(state: &AppState) -> serde_json::Value {
    json!({
        "enabled": tune_core::cover_plus::actif_globalement(&state.backend),
        "artist_photo": tune_core::cover_plus::photo_artiste_active(&state.backend),
    })
}

async fn lire_le_serveur(State(state): State<AppState>) -> Response {
    Json(etat_du_serveur(&state)).into_response()
}

async fn regler_le_serveur(
    State(state): State<AppState>,
    Json(corps): Json<CorpsInterrupteur>,
) -> Response {
    let mut resultat = Ok(());
    if let Some(actif) = corps.enabled {
        resultat = tune_core::cover_plus::regler_globalement(&state.backend, actif);
    }
    if resultat.is_ok()
        && let Some(photo) = corps.artist_photo
    {
        resultat = tune_core::cover_plus::regler_photo_artiste(&state.backend, photo);
    }
    match resultat {
        Ok(()) => Json(etat_du_serveur(&state)).into_response(),
        Err(e) => {
            tracing::warn!(erreur = %e, "cover_plus_reglage_serveur_echec");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/cover-plus", get(lire_le_serveur).put(regler_le_serveur))
        .route("/{id}/cover-plus", get(lire).put(regler))
}

#[cfg(test)]
#[path = "radios_cover_plus_8452_tests.rs"]
mod tests;
