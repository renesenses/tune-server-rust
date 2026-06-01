use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::history_repo::HistoryRepo;

use crate::error::AppError;
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

async fn top_artists(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
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

async fn top_tracks(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
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

async fn top_albums(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
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
        .map(|(day, play_count, total_ms)| {
            json!({
                "day": day,
                "play_count": play_count,
                "total_listened_ms": total_ms,
                "hours": (total_ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
            })
        })
        .collect();
    Json(json!(items))
}

async fn genre_breakdown(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    // Collect raw genre + genres columns from tracks
    let raw: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).unwrap_or(None),
                    row.get::<_, Option<String>>(1).unwrap_or(None),
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    // Split multi-genre values and count individual genres
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (genre_col, genres_col) in &raw {
        let mut genres_for_track: Vec<String> = Vec::new();
        if let Some(json_str) = genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                genres_for_track = arr
                    .into_iter()
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
            }
        }
        if genres_for_track.is_empty() {
            if let Some(raw_genre) = genre_col {
                genres_for_track = tune_core::metadata::split_genre_tag(raw_genre);
            }
        }
        for g in genres_for_track {
            *counts.entry(g).or_insert(0) += 1;
        }
    }

    // Sort by count descending, limit to 30
    let mut sorted: Vec<(String, i64)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(30);

    let items: Vec<serde_json::Value> = sorted
        .iter()
        .map(|(genre, count)| serde_json::json!({ "genre": genre, "count": count }))
        .collect();

    Ok(Json(serde_json::json!(items)))
}
