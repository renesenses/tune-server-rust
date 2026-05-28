use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

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

/// Get the current Spotify access token from the service registry via save_tokens().
async fn spotify_token(state: &AppState) -> Option<String> {
    let registry = state.services.lock().await;
    let svc = registry.get("spotify")?;
    drop(registry); // release registry lock before awaiting
    let svc = svc.lock().await;
    let tokens = svc.save_tokens()?;
    tokens.get("access_token")?.as_str().map(Into::into)
}

async fn connect_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("spotify_connect_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let has_token = spotify_token(&state).await.is_some();
    Json(json!({
        "enabled": enabled,
        "authenticated": has_token,
    }))
}

async fn enable_connect(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("spotify_connect_enabled", "true").ok();
    Json(json!({"enabled": true}))
}

async fn disable_connect(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let resp = client
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let payload = json!({
        "device_ids": [body.device_id],
        "play": body.play.unwrap_or(true),
    });
    let resp = client
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
