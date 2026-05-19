use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/scan", post(scan_devices))
}

async fn list_devices(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
    }))
}

async fn scan_devices(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "scanning",
        "message": "discovery not yet wired to API",
    }))
}
