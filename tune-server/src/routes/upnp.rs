use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(upnp_status))
        .route("/config", get(upnp_config).post(set_upnp_config))
}

/// GET /api/v1/upnp/status — report whether the UPnP MediaServer is active.
async fn upnp_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let upnp = state.upnp.as_ref();
    let enabled = upnp.is_some();
    let uuid = upnp.map(|u| u.uuid.clone()).unwrap_or_default();
    let friendly_name = upnp
        .map(|u| u.friendly_name.clone())
        .unwrap_or_else(|| "Tune Server".into());

    Json(json!({
        "enabled": enabled,
        "uuid": uuid,
        "friendly_name": friendly_name,
        "port": state.port,
    }))
}

/// GET /api/v1/upnp/config — return the current UPnP configuration.
async fn upnp_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("upnp_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(true);
    let friendly_name = settings
        .get("upnp_friendly_name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Tune Server".into());

    Json(json!({
        "enabled": enabled,
        "friendly_name": friendly_name,
    }))
}

/// POST /api/v1/upnp/config — update UPnP configuration.
async fn set_upnp_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());

    if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
        settings.set("upnp_enabled", if enabled { "true" } else { "false" }).ok();
    }
    if let Some(name) = body.get("friendly_name").and_then(|v| v.as_str()) {
        settings.set("upnp_friendly_name", name).ok();
    }

    Json(json!({"ok": true}))
}
