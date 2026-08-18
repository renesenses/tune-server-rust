//! AirPlay 2 PIN pairing endpoints (#1135 — Samsung/LG AirPlay-2-only TVs).
//!
//! Most AirPlay receivers accept the hard-coded transient PIN and pair
//! automatically when playback starts. AirPlay-2-only TVs (Samsung, LG) and
//! Apple TV instead require HomeKit PIN pairing: the receiver displays a
//! 4-digit code the user must type back. These endpoints drive that flow:
//!
//!   1. `POST /outputs/{device_id}/airplay/pair-start` → tells the daemon to
//!      begin PIN pairing; the receiver shows a code on screen.
//!   2. Client polls `GET /outputs/{device_id}/airplay/pair-status` until it
//!      reads `pin_requested`, then prompts the user for the code.
//!   3. `POST /outputs/{device_id}/airplay/pair-pin` with `{"pin":"1234"}` →
//!      submits the code; status transitions to `connected` (or `failed:…`).
//!
//! The pairing methods fire-and-return so a slow 30s pairing handshake never
//! blocks the shared output mutex (status polling stays responsive).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use tune_core::outputs::OutputTarget;
use tune_core::outputs::airplay2::Airplay2Output;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{device_id}/airplay/pair-start", post(pair_start_handler))
        .route("/{device_id}/airplay/pair-status", get(pair_status_handler))
        .route("/{device_id}/airplay/pair-pin", post(pair_pin_handler))
}

/// Fetch an output by id, or 404 if unknown.
async fn lookup_output(
    state: &AppState,
    device_id: &str,
) -> Result<Arc<Mutex<Box<dyn OutputTarget>>>, AppError> {
    let registry = state.outputs.lock().await;
    registry
        .get(device_id)
        .ok_or_else(|| AppError::not_found("output not found"))
}

/// `POST /outputs/{device_id}/airplay/pair-start` — begin PIN pairing.
///
/// The receiver displays a 4-digit code. Fire-and-return: the client then
/// polls `pair-status` and submits the code via `pair-pin`.
async fn pair_start_handler(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let output = lookup_output(&state, &device_id).await?;
    let guard = output.lock().await;
    let ap2 = guard
        .as_any()
        .downcast_ref::<Airplay2Output>()
        .ok_or_else(|| AppError::bad_request("output is not an AirPlay 2 receiver"))?;
    ap2.start_pin_pairing().await.map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true, "status": "pin_requested"})).into_response())
}

/// `GET /outputs/{device_id}/airplay/pair-status` — poll the pairing phase.
///
/// Returns one of `idle`, `pin_requested`, `connected`, `failed:<msg>`.
async fn pair_status_handler(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let output = lookup_output(&state, &device_id).await?;
    let guard = output.lock().await;
    let ap2 = guard
        .as_any()
        .downcast_ref::<Airplay2Output>()
        .ok_or_else(|| AppError::bad_request("output is not an AirPlay 2 receiver"))?;
    let status = ap2.pairing_status().await;
    Ok(Json(json!({"status": status})).into_response())
}

/// Request body for submitting the PIN code shown on the receiver.
#[derive(Deserialize)]
struct PairPinBody {
    pin: String,
}

/// `POST /outputs/{device_id}/airplay/pair-pin` — submit the 4-digit code.
async fn pair_pin_handler(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<PairPinBody>,
) -> Result<impl IntoResponse, AppError> {
    let pin = body.pin.trim().to_string();
    if pin.is_empty() {
        return Err(AppError::bad_request("pin is empty"));
    }
    let output = lookup_output(&state, &device_id).await?;
    let guard = output.lock().await;
    let ap2 = guard
        .as_any()
        .downcast_ref::<Airplay2Output>()
        .ok_or_else(|| AppError::bad_request("output is not an AirPlay 2 receiver"))?;
    ap2.submit_pin(&pin).await.map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}
