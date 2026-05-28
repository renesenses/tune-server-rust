use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(roon_status))
        .route("/config", get(roon_config).post(set_roon_config))
        .route("/zones", get(roon_zones))
}

/// Roon Bridge status — Roon uses proprietary RAAT (Roon Advanced Audio Transport) protocol.
async fn roon_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("roon_bridge_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let core_host = settings.get("roon_core_host").ok().flatten();

    Json(json!({
        "available": false,
        "enabled": enabled,
        "connected": false,
        "core_host": core_host,
        "protocol": "RAAT",
        "message": "Roon Bridge integration requires the proprietary RAAT protocol. \
                    This is a stub — Roon uses a closed protocol that requires licensing from Roon Labs.",
        "requirements": [
            "Roon Core running on the network",
            "RAAT protocol implementation (proprietary, requires Roon Labs partnership)",
            "Network discovery via SSDP/mDNS",
        ],
    }))
}

/// Get Roon Bridge configuration.
async fn roon_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("roon_bridge_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let core_host = settings
        .get("roon_core_host")
        .ok()
        .flatten()
        .unwrap_or_default();
    let display_name = settings
        .get("roon_display_name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Tune".into());

    Json(json!({
        "roon_bridge_enabled": enabled,
        "roon_core_host": core_host,
        "roon_display_name": display_name,
    }))
}

#[derive(Deserialize)]
struct RoonConfigBody {
    roon_bridge_enabled: Option<bool>,
    roon_core_host: Option<String>,
    roon_display_name: Option<String>,
}

/// Set Roon Bridge configuration.
async fn set_roon_config(
    State(state): State<AppState>,
    Json(body): Json<RoonConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());

    if let Some(v) = body.roon_bridge_enabled {
        settings
            .set("roon_bridge_enabled", if v { "true" } else { "false" })
            .ok();
    }
    if let Some(host) = &body.roon_core_host {
        settings.set("roon_core_host", host).ok();
    }
    if let Some(name) = &body.roon_display_name {
        settings.set("roon_display_name", name).ok();
    }

    Json(json!({"saved": true}))
}

/// List Roon zones (stub — requires RAAT protocol).
async fn roon_zones() -> Json<Value> {
    Json(json!({
        "zones": [],
        "message": "Zone enumeration requires active RAAT connection to a Roon Core. \
                    This is a stub implementation.",
    }))
}
