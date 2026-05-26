use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

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

async fn party_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let enabled = settings.get("party_mode")
        .ok().flatten().map(|v| v == "true").unwrap_or(false);
    Json(json!({
        "enabled": enabled,
        "queue": [],
        "participants": 0,
    }))
}

async fn enable_party(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("party_mode", "true").ok();
    Json(json!({"enabled": true}))
}

async fn disable_party(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("party_mode", "false").ok();
    Json(json!({"enabled": false}))
}

#[derive(Deserialize)]
struct PartyAddRequest {
    track_id: Option<i64>,
    source: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    added_by: Option<String>,
}

async fn party_add(Json(body): Json<PartyAddRequest>) -> Json<Value> {
    Json(json!({
        "added": true,
        "track_id": body.track_id,
        "title": body.title,
        "artist_name": body.artist_name,
        "added_by": body.added_by.unwrap_or_else(|| "anonymous".into()),
        "position": 0,
    }))
}

async fn party_queue() -> Json<Value> {
    Json(json!({
        "queue": [],
        "total": 0,
    }))
}

#[derive(Deserialize)]
struct VoteRequest {
    track_id: i64,
    vote: i32,
}

async fn party_vote(Json(body): Json<VoteRequest>) -> Json<Value> {
    Json(json!({
        "track_id": body.track_id,
        "vote": body.vote,
        "total_votes": body.vote,
    }))
}

async fn party_vote_reset() -> Json<Value> {
    Json(json!({"reset": true}))
}
