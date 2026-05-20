use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::history_repo::HistoryRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct HistoryParams {
    limit: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(recent_history))
        .route("/top-tracks", get(top_tracks))
        .route("/top-artists", get(top_artists))
        .route("/dashboard", get(dashboard))
}

async fn recent_history(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let repo = HistoryRepo::new(state.db);
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}

async fn top_tracks(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items: Vec<Value> = repo
        .top_tracks(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({ "title": title, "artist_name": artist, "plays": plays })
        })
        .collect();
    Json(json!(items))
}

async fn top_artists(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items: Vec<Value> = repo
        .top_artists(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, plays)| json!({ "name": name, "plays": plays }))
        .collect();
    Json(json!(items))
}

async fn dashboard(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::new(state.db);
    match repo.dashboard() {
        Ok(stats) => Json(json!(stats)),
        Err(_) => Json(json!({
            "total_listens": 0,
            "total_duration_ms": 0,
            "unique_tracks": 0,
            "unique_artists": 0,
        })),
    }
}
