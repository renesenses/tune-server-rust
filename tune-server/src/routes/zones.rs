use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::zone_repo::ZoneRepo;
use tune_core::db::play_queue_repo::PlayQueueRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateZone {
    name: String,
    output_type: Option<String>,
    output_device_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateVolume {
    volume: i32,
}

#[derive(Deserialize)]
struct UpdateMuted {
    muted: bool,
}

#[derive(Deserialize)]
struct RenameZone {
    name: String,
}

#[derive(Deserialize)]
struct SetQueue {
    track_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct SetCurrent {
    position: i64,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_zones).post(create_zone))
        .route("/{id}", get(get_zone).delete(delete_zone))
        .route("/{id}/volume", put(update_volume))
        .route("/{id}/muted", put(update_muted))
        .route("/{id}/name", put(rename_zone))
        .route("/{id}/queue", get(get_queue).post(set_queue))
        .route("/{id}/queue/current", put(set_current))
        .route("/{id}/queue/clear", post(clear_queue))
}

async fn list_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = ZoneRepo::new(state.db);
    let zones = repo.list().unwrap_or_default();
    Json(json!({ "items": zones, "total": zones.len() }))
}

async fn get_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(zone)) => Json(json!(zone)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_zone(
    State(state): State<AppState>,
    Json(body): Json<CreateZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.create(&body.name, body.output_type.as_deref(), body.output_device_id.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateVolume>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_volume(id, body.volume) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_muted(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMuted>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_muted(id, body.muted) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn rename_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_name(id, &body.name) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_queue(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let repo = PlayQueueRepo::new(state.db);
    let items = repo.get_queue(zone_id).unwrap_or_default();
    let current = items.iter().find(|i| i.is_current).cloned();
    Json(json!({ "items": items, "current": current, "total": items.len() }))
}

async fn set_queue(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SetQueue>,
) -> impl IntoResponse {
    let repo = PlayQueueRepo::new(state.db);
    match repo.set_queue(zone_id, &body.track_ids) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn set_current(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SetCurrent>,
) -> impl IntoResponse {
    let repo = PlayQueueRepo::new(state.db);
    match repo.set_current(zone_id, body.position) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn clear_queue(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlayQueueRepo::new(state.db);
    match repo.clear(zone_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
