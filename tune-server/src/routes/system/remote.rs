use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub(super) async fn get_remote_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let url = settings
        .get("remote_server_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    let enabled = settings
        .get("server_mode")
        .ok()
        .flatten()
        .map(|m| m == "remote")
        .unwrap_or(false);
    Json(json!({
        "enabled": enabled,
        "remote_url": url,
    }))
}

#[derive(Deserialize)]
pub(super) struct RemoteConfig {
    remote_url: String,
}

pub(super) async fn set_remote_config(
    State(state): State<AppState>,
    Json(body): Json<RemoteConfig>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("remote_server_url", &body.remote_url).ok();
    settings.set("server_mode", "remote").ok();
    Json(json!({"enabled": true, "remote_url": body.remote_url}))
}

pub(super) async fn remote_status(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let url = settings
        .get("remote_server_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    if url.is_empty() {
        return Json(json!({"connected": false, "error": "no remote URL configured"}))
            .into_response();
    }

    match state
        .http_client
        .get(format!("{url}/api/v1/system/health"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(json!({"connected": true, "remote_url": url, "remote_health": data}))
                .into_response()
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            Json(json!({
                "connected": false,
                "remote_url": url,
                "error": format!("remote returned HTTP {status_code}"),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "connected": false,
            "remote_url": url,
            "error": format!("unreachable: {e}"),
        }))
        .into_response(),
    }
}
