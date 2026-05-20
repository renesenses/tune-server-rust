use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins))
        .route("/{name}", get(get_plugin))
        .route("/{name}/enable", post(enable_plugin))
        .route("/{name}/disable", post(disable_plugin))
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

async fn get_plugin(Path(name): Path<String>) -> Json<Value> {
    Json(json!({
        "name": name,
        "status": "not_installed",
        "message": "plugin system pending (Phase 8)",
    }))
}

async fn enable_plugin(Path(name): Path<String>) -> Json<Value> {
    Json(json!({ "name": name, "enabled": true }))
}

async fn disable_plugin(Path(name): Path<String>) -> Json<Value> {
    Json(json!({ "name": name, "enabled": false }))
}
