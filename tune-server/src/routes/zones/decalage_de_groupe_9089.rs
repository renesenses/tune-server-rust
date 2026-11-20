//! #9089 (Jon, fil 2757) — `GET` / `PUT /zones/{id}/group-offset` : le
//! décalage manuel (ms, signé) d'une zone quand elle suit la meneuse de son
//! groupe. La règle et son effet : [`tune_core::zones::decalage_de_groupe_9089`].

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::zones::decalage_de_groupe_9089::{self as regle, DECALAGE_MAX_MS, RefusDuDecalage};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(super) struct CorpsDuDecalage {
    offset_ms: i64,
}

fn reponse(zone_id: i64, decalage_ms: i64) -> Response {
    Json(json!({
        "zone_id": zone_id,
        "offset_ms": decalage_ms,
        "min_ms": -DECALAGE_MAX_MS,
        "max_ms": DECALAGE_MAX_MS,
    }))
    .into_response()
}

fn zone_inconnue(state: &AppState, zone_id: i64) -> Option<Response> {
    match ZoneRepo::with_backend(state.backend.clone()).get(zone_id) {
        Ok(Some(_)) => None,
        _ => Some(
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone_not_found", "zone_id": zone_id})),
            )
                .into_response(),
        ),
    }
}

pub(super) async fn lire(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Response {
    if let Some(refus) = zone_inconnue(&state, zone_id) {
        return refus;
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reponse(zone_id, regle::lire(&reglages, zone_id))
}

pub(super) async fn ecrire(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(corps): Json<CorpsDuDecalage>,
) -> Response {
    if let Some(refus) = zone_inconnue(&state, zone_id) {
        return refus;
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    match regle::ecrire(&reglages, zone_id, corps.offset_ms) {
        Ok(decalage_ms) => {
            tracing::info!(zone_id, decalage_ms, "zone_group_offset_set");
            reponse(zone_id, decalage_ms)
        }
        Err(RefusDuDecalage::HorsBornes(ms)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "group_offset_out_of_range",
                "offset_ms": ms,
                "min_ms": -DECALAGE_MAX_MS,
                "max_ms": DECALAGE_MAX_MS,
            })),
        )
            .into_response(),
        Err(RefusDuDecalage::Ecriture(message)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "group_offset_write_failed", "message": message})),
        )
            .into_response(),
    }
}
