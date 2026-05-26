use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins))
        .route("/docs", get(plugin_docs))
        .route("/{name}", get(get_plugin))
        .route("/{name}", axum::routing::delete(delete_plugin))
        .route("/{name}/enable", post(enable_plugin))
        .route("/{name}/disable", post(disable_plugin))
        .route("/{name}/install", post(install_plugin))
        .route("/{name}/update", post(update_plugin))
}

async fn list_plugins(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let plugins: Vec<Value> = settings
        .get("plugins")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(plugins))
}

async fn get_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_installed");
    let installed = settings
        .get(&key)
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    let enabled_key = format!("plugin_{name}_enabled");
    let enabled = settings
        .get(&enabled_key)
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "name": name,
        "installed": installed,
        "enabled": enabled,
        "status": if installed { "installed" } else { "not_installed" },
    }))
}

async fn enable_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_enabled");
    settings.set(&key, "true").ok();
    Json(json!({ "name": name, "enabled": true }))
}

async fn disable_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_enabled");
    settings.set(&key, "false").ok();
    Json(json!({ "name": name, "enabled": false }))
}

#[derive(Deserialize)]
struct InstallRequest {
    #[allow(dead_code)]
    version: Option<String>,
}

async fn install_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
    Json(_body): Json<InstallRequest>,
) -> Json<Value> {
    // Stub: Rust server doesn't use pip. Track state in settings.
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_installed");
    settings.set(&key, "true").ok();
    let enabled_key = format!("plugin_{name}_enabled");
    settings.set(&enabled_key, "true").ok();
    Json(json!({ "name": name, "status": "installed" }))
}

async fn update_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    // Stub: Rust server doesn't use pip. Track state in settings.
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_installed");
    settings.set(&key, "true").ok();
    Json(json!({ "name": name, "status": "updated" }))
}

async fn delete_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let key = format!("plugin_{name}_installed");
    settings.delete(&key).ok();
    let enabled_key = format!("plugin_{name}_enabled");
    settings.delete(&enabled_key).ok();
    StatusCode::NO_CONTENT
}

async fn plugin_docs() -> Json<Value> {
    Json(json!({ "url": "https://mozaiklabs.fr/docs/plugins" }))
}
