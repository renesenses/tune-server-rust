use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list_peers))
}

async fn list_peers() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "mDNS peer discovery via _tune-server._tcp",
    }))
}
