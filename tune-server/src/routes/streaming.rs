use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/{service}/status", get(service_status))
        .route("/{service}/auth", post(service_auth))
        .route("/{service}/logout", post(service_logout))
        .route("/{service}/search", get(service_search))
}

async fn list_services() -> Json<Value> {
    Json(json!({
        "services": [
            { "name": "tidal", "enabled": false, "authenticated": false },
            { "name": "qobuz", "enabled": false, "authenticated": false },
            { "name": "spotify", "enabled": false, "authenticated": false },
            { "name": "deezer", "enabled": false, "authenticated": false },
            { "name": "youtube", "enabled": false, "authenticated": false },
            { "name": "amazon_music", "enabled": false, "authenticated": false },
        ]
    }))
}

async fn service_status(Path(service): Path<String>) -> Json<Value> {
    Json(json!({
        "service": service,
        "enabled": false,
        "authenticated": false,
    }))
}

async fn service_auth(Path(service): Path<String>) -> Json<Value> {
    Json(json!({
        "service": service,
        "status": "not_implemented",
        "message": "streaming auth not yet wired",
    }))
}

async fn service_logout(Path(service): Path<String>) -> Json<Value> {
    Json(json!({
        "service": service,
        "status": "logged_out",
    }))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<i64>,
}

async fn service_search(
    Path(service): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    Json(json!({
        "service": service,
        "query": q.q,
        "tracks": [],
        "albums": [],
        "artists": [],
    }))
}
