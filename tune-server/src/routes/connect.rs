use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        // Qobuz Connect
        .route("/qobuz/status", get(qobuz_connect_status))
        .route("/qobuz/enable", post(qobuz_connect_enable))
        .route("/qobuz/disable", post(qobuz_connect_disable))
        // Tidal Connect
        .route("/tidal/status", get(tidal_connect_status))
        .route("/tidal/enable", post(tidal_connect_enable))
        .route("/tidal/disable", post(tidal_connect_disable))
}

// --- Qobuz Connect ---

/// Qobuz Connect status — requires Qobuz partner API access.
async fn qobuz_connect_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("qobuz_connect_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "available": false,
        "enabled": enabled,
        "connected": false,
        "message": "Qobuz Connect requires partner API access reserved for commercial partners. \
                    Contact Qobuz for partnership details.",
        "protocol": "Qobuz Connect (proprietary)",
        "requirements": [
            "Qobuz commercial partner agreement",
            "Partner API credentials",
            "Device certification from Qobuz",
        ],
    }))
}

/// Enable Qobuz Connect (stores preference — actual functionality requires partner API).
async fn qobuz_connect_enable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("qobuz_connect_enabled", "true").ok();
    Json(json!({
        "enabled": true,
        "message": "Qobuz Connect preference saved. Actual functionality requires partner API access.",
    }))
}

/// Disable Qobuz Connect.
async fn qobuz_connect_disable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("qobuz_connect_enabled", "false").ok();
    Json(json!({
        "enabled": false,
        "message": "Qobuz Connect disabled.",
    }))
}

// --- Tidal Connect ---

/// Tidal Connect status — requires Tidal partner API access.
async fn tidal_connect_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("tidal_connect_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "available": false,
        "enabled": enabled,
        "connected": false,
        "message": "Tidal Connect requires partner integration via the Tidal SDK. \
                    Access is restricted to certified hardware partners.",
        "protocol": "Tidal Connect SDK (proprietary)",
        "requirements": [
            "Tidal hardware partner certification",
            "Tidal Connect SDK license",
            "Device registration with Tidal",
        ],
    }))
}

/// Enable Tidal Connect (stores preference — actual functionality requires partner SDK).
async fn tidal_connect_enable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("tidal_connect_enabled", "true").ok();
    Json(json!({
        "enabled": true,
        "message": "Tidal Connect preference saved. Actual functionality requires Tidal SDK partner access.",
    }))
}

/// Disable Tidal Connect.
async fn tidal_connect_disable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("tidal_connect_enabled", "false").ok();
    Json(json!({
        "enabled": false,
        "message": "Tidal Connect disabled.",
    }))
}
