use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list_peers))
}

async fn list_peers(State(state): State<AppState>) -> Json<Value> {
    let peers = state.discovered_tune_peers().await;
    let total = peers.len();
    Json(json!({
        "items": peers,
        "total": total,
        "discovery": "_tune-server._tcp",
    }))
}
