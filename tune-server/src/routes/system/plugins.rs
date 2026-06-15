use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub(super) async fn list_system_plugins(State(state): State<AppState>) -> Json<Value> {
    // Alias for /plugins list
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let plugins: Vec<Value> = settings
        .get("plugins")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(plugins))
}
