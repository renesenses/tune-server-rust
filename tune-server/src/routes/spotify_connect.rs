use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(connect_status))
        .route("/enable", post(enable_connect))
        .route("/disable", post(disable_connect))
        .route("/devices", get(list_connect_devices))
        .route("/transfer", post(transfer_playback))
}

async fn spotify_token(state: &AppState) -> Option<String> {
    let registry = state.services.lock().await;
    let svc = registry.get("spotify")?;
    drop(registry);
    let svc = svc.lock().await;
    let tokens = svc.save_tokens()?;
    tokens.get("access_token")?.as_str().map(Into::into)
}

async fn connect_status(State(state): State<AppState>) -> Json<Value> {
    Json(state.spotify_connect.status().await)
}

#[derive(Deserialize)]
struct EnableBody {
    zone_id: Option<i64>,
    device_name: Option<String>,
}

async fn enable_connect(
    State(state): State<AppState>,
    Json(body): Json<EnableBody>,
) -> Json<Value> {
    let zone_id = body.zone_id.unwrap_or(1);

    if let Some(ref name) = body.device_name {
        info!(name, "spotify_connect_custom_name");
    }

    if let Err(e) = state.spotify_connect.enable(zone_id).await {
        return Json(json!({"enabled": false, "error": e}));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("spotify_connect_enabled", "true").ok();
    settings
        .set("spotify_connect_zone_id", &zone_id.to_string())
        .ok();

    Json(json!({"enabled": true, "zone_id": zone_id}))
}

async fn disable_connect(State(state): State<AppState>) -> Json<Value> {
    state.spotify_connect.disable().await;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("spotify_connect_enabled", "false").ok();

    Json(json!({"enabled": false}))
}

async fn list_connect_devices(State(state): State<AppState>) -> impl IntoResponse {
    let Some(token) = spotify_token(&state).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Spotify not authenticated"})),
        )
            .into_response();
    };
    let resp = state
        .http_client
        .get("https://api.spotify.com/v1/me/player/devices")
        .bearer_auth(&token)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let body: Value = r.json().await.unwrap_or(json!({"devices": []}));
            Json(body.get("devices").cloned().unwrap_or(json!([]))).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Spotify API error: {e}")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct TransferBody {
    device_id: String,
    play: Option<bool>,
}

async fn transfer_playback(
    State(state): State<AppState>,
    Json(body): Json<TransferBody>,
) -> impl IntoResponse {
    let Some(token) = spotify_token(&state).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Spotify not authenticated"})),
        )
            .into_response();
    };
    let payload = json!({
        "device_ids": [body.device_id],
        "play": body.play.unwrap_or(true),
    });
    let resp = state
        .http_client
        .put("https://api.spotify.com/v1/me/player")
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            if status == 204 || status == 200 {
                Json(json!({"status": "transferred", "device_id": body.device_id})).into_response()
            } else {
                let err: Value = r.json().await.unwrap_or(json!({"error": "unknown"}));
                (StatusCode::BAD_GATEWAY, Json(err)).into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Spotify API error: {e}")})),
        )
            .into_response(),
    }
}

pub async fn auto_start(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get("spotify_connect_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let zone_id: i64 = settings
        .get("spotify_connect_zone_id")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    if !tune_core::streaming::spotify_connect::binary_available() {
        info!("spotify_connect_auto_start_skipped: librespot not found");
        return;
    }

    match state.spotify_connect.enable(zone_id).await {
        Ok(()) => info!(zone_id, "spotify_connect_auto_started"),
        Err(e) => info!(error = %e, "spotify_connect_auto_start_failed"),
    }
}
