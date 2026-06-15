use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(party_status))
        .route("/enable", post(enable_party))
        .route("/disable", post(disable_party))
        .route("/add", post(party_add))
        .route("/queue", get(party_queue))
        .route("/vote", post(party_vote))
        .route("/vote/reset", post(party_vote_reset))
}

// --- Room handlers (used by mod.rs router at /party/rooms) ---

pub async fn list_rooms(State(state): State<AppState>) -> Json<Value> {
    let mgr = state.rooms.lock().await;
    Json(json!({ "rooms": mgr.list_rooms() }))
}

pub async fn create_room(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body["room_id"].as_str().unwrap_or("").to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            json!({"error": "room_id required"}).to_string(),
        )
            .into_response();
    }
    let mut mgr = state.rooms.lock().await;
    if mgr.create_room(&id) {
        (StatusCode::CREATED, Json(json!({"room_id": id}))).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            json!({"error": "room exists"}).to_string(),
        )
            .into_response()
    }
}

pub async fn room_info(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let mgr = state.rooms.lock().await;
    match mgr.room_info(&id) {
        Some(info) => Json(info).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn delete_room(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    let mut mgr = state.rooms.lock().await;
    if mgr.delete_room(&id) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

fn load_queue(settings: &SettingsRepo) -> Vec<Value> {
    settings
        .get("party_queue")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_queue(settings: &SettingsRepo, queue: &[Value]) {
    if let Ok(serialized) = serde_json::to_string(queue) {
        settings.set("party_queue", &serialized).ok();
    }
}

async fn party_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get("party_mode")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let queue = load_queue(&settings);
    Json(json!({
        "enabled": enabled,
        "queue": queue,
        "queue_length": queue.len(),
    }))
}

async fn enable_party(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("party_mode", "true").ok();
    settings.set("party_queue", "[]").ok();
    Json(json!({"enabled": true}))
}

async fn disable_party(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("party_mode", "false").ok();
    settings.set("party_queue", "[]").ok();
    Json(json!({"enabled": false}))
}

#[derive(Deserialize)]
struct PartyAddRequest {
    track_id: Option<i64>,
    title: Option<String>,
    artist_name: Option<String>,
    added_by: Option<String>,
}

async fn party_add(
    State(state): State<AppState>,
    Json(body): Json<PartyAddRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut queue = load_queue(&settings);
    let entry = json!({
        "track_id": body.track_id,
        "title": body.title,
        "artist_name": body.artist_name,
        "added_by": body.added_by.as_deref().unwrap_or("anonymous"),
        "votes": 0,
        "position": queue.len(),
    });
    queue.push(entry.clone());
    save_queue(&settings, &queue);
    Json(json!({"added": true, "entry": entry, "queue_length": queue.len()}))
}

async fn party_queue(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let queue = load_queue(&settings);
    Json(json!({"queue": queue, "total": queue.len()}))
}

#[derive(Deserialize)]
struct VoteRequest {
    track_id: i64,
    vote: i32,
}

async fn party_vote(State(state): State<AppState>, Json(body): Json<VoteRequest>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut queue = load_queue(&settings);
    let mut total_votes = 0;
    for item in &mut queue {
        if item["track_id"].as_i64() == Some(body.track_id) {
            let current = item["votes"].as_i64().unwrap_or(0);
            let new_votes = current + body.vote as i64;
            item["votes"] = json!(new_votes);
            total_votes = new_votes;
        }
    }
    queue.sort_by(|a, b| {
        b["votes"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["votes"].as_i64().unwrap_or(0))
    });
    save_queue(&settings, &queue);
    Json(json!({"track_id": body.track_id, "vote": body.vote, "total_votes": total_votes}))
}

async fn party_vote_reset(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut queue = load_queue(&settings);
    for item in &mut queue {
        item["votes"] = json!(0);
    }
    save_queue(&settings, &queue);
    Json(json!({"reset": true, "queue_length": queue.len()}))
}
