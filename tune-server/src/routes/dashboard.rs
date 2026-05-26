use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::history_repo::HistoryRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct DashParams {
    limit: Option<i64>,
    days: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(dashboard_stats))
        .route("/top-artists", get(top_artists))
        .route("/top-albums", get(top_albums))
        .route("/top-tracks", get(top_tracks))
        .route("/genre-breakdown", get(genre_breakdown))
        .route("/listening-history", get(listening_history))
}

async fn dashboard_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::new(state.db);
    match repo.dashboard() {
        Ok(s) => Json(json!(s)),
        Err(_) => Json(json!({
            "total_listens": 0,
            "total_duration_ms": 0,
            "unique_tracks": 0,
            "unique_artists": 0,
        })),
    }
}

async fn top_artists(
    State(state): State<AppState>,
    Query(p): Query<DashParams>,
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

async fn top_tracks(
    State(state): State<AppState>,
    Query(p): Query<DashParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items: Vec<Value> = repo
        .top_tracks(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| json!({ "title": title, "artist_name": artist, "plays": plays }))
        .collect();
    Json(json!(items))
}

async fn top_albums(
    State(state): State<AppState>,
    Query(p): Query<DashParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items: Vec<Value> = repo
        .top_albums(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| json!({ "album_title": title, "artist_name": artist, "plays": plays }))
        .collect();
    Json(json!(items))
}

async fn listening_history(
    State(state): State<AppState>,
    Query(p): Query<DashParams>,
) -> Json<Value> {
    let days = p.days.unwrap_or(30);
    let repo = HistoryRepo::new(state.db);
    let items: Vec<Value> = repo
        .listening_history(days)
        .unwrap_or_default()
        .into_iter()
        .map(|(day, play_count, total_ms)| json!({
            "day": day,
            "play_count": play_count,
            "total_listened_ms": total_ms,
            "hours": (total_ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
        }))
        .collect();
    Json(json!(items))
}

async fn genre_breakdown(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT genre, COUNT(*) as count FROM tracks WHERE genre IS NOT NULL AND genre != '' GROUP BY genre ORDER BY count DESC LIMIT 30")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "genre": row.get::<_, Option<String>>(0).ok().flatten(),
                    "count": row.get::<_, i64>(1).unwrap_or(0),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}
