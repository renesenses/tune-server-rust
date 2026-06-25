use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
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
        .route("/wrapped", get(wrapped))
}

async fn dashboard_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.dashboard() {
        Ok(s) => Json(json!(s)),
        Err(e) => {
            tracing::warn!(error = %e, "dashboard_stats_error");
            Json(json!({
                "total_listens": 0,
                "total_duration_ms": 0,
                "unique_tracks": 0,
                "unique_artists": 0,
            }))
        }
    }
}

async fn top_artists(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
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
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.top_tracks(limit).unwrap_or_default();
    Json(json!(items))
}

async fn top_albums(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
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
    let repo = HistoryRepo::with_backend(state.backend.clone());
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
    let rows = state
        .backend
        .query_many(
            "SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;

    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for cols in &rows {
        let genre_col = cols.first().and_then(|v| v.as_string());
        let genres_col = cols.get(1).and_then(|v| v.as_string());

        let mut genres_for_track: Vec<String> = Vec::new();
        if let Some(json_str) = &genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                genres_for_track = arr
                    .into_iter()
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
            }
        }
        if genres_for_track.is_empty() {
            if let Some(raw_genre) = &genre_col {
                genres_for_track = tune_core::metadata::split_genre_tag(raw_genre);
            }
        }
        for g in genres_for_track {
            *counts.entry(g).or_insert(0) += 1;
        }
    }

    let mut sorted: Vec<(String, i64)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(30);

    let items: Vec<Value> = sorted
        .iter()
        .map(|(genre, count)| json!({ "genre": genre, "count": count }))
        .collect();

    Ok(Json(json!(items)))
}

fn is_consecutive_days(a: &str, b: &str) -> bool {
    fn to_days(s: &str) -> Option<i64> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let d: i64 = parts[2].parse().ok()?;
        Some(y * 366 + m * 31 + d)
    }
    match (to_days(a), to_days(b)) {
        (Some(da), Some(db)) => db - da == 1,
        _ => false,
    }
}

#[derive(Deserialize)]
struct WrappedParams {
    year: Option<i32>,
}

async fn wrapped(
    State(state): State<AppState>,
    Query(p): Query<WrappedParams>,
) -> Result<Json<Value>, AppError> {
    let year = p.year.unwrap_or(2026);
    let year_start = format!("{year}-01-01");
    let year_end = format!("{}-01-01", year + 1);
    let b = &state.backend;

    let date_trunc_day = |col: &str| match b.engine() {
        Engine::Sqlite => SqliteDialect.date_trunc_day(col),
        Engine::Postgres => PostgresDialect.date_trunc_day(col),
    };

    let row = b
        .query_one(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM listen_history WHERE listened_at >= ? AND listened_at < ?",
            &[&year_start as &dyn tune_core::db::backend::ToSqlValue, &year_end],
        )
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();
    let total_listens = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_hours = (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;

    let top_artists: Vec<Value> = b
        .query_many(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? AND artist_name IS NOT NULL \
             GROUP BY artist_name ORDER BY plays DESC LIMIT 10",
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|cols| {
            json!({
                "artist": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let top_tracks: Vec<Value> = b
        .query_many(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? \
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT 10",
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|cols| {
            json!({
                "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let day_expr = date_trunc_day("listened_at");
    let days_sql = format!(
        "SELECT DISTINCT {day_expr} as d FROM listen_history \
         WHERE listened_at >= ? AND listened_at < ? ORDER BY 1"
    );
    let days: Vec<String> = b
        .query_many(
            &days_sql,
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();

    let mut max_streak = if days.is_empty() { 0u32 } else { 1u32 };
    let mut current_streak = 1u32;
    for w in days.windows(2) {
        if is_consecutive_days(&w[0], &w[1]) {
            current_streak += 1;
        } else {
            max_streak = max_streak.max(current_streak);
            current_streak = 1;
        }
    }
    max_streak = max_streak.max(current_streak);

    let stats_row = b
        .query_one(
            "SELECT COUNT(DISTINCT artist_name), COUNT(DISTINCT COALESCE(title, '') || COALESCE(artist_name, '')) \
             FROM listen_history WHERE listened_at >= ? AND listened_at < ?",
            &[&year_start as &dyn tune_core::db::backend::ToSqlValue, &year_end],
        )
        .unwrap_or(None)
        .unwrap_or_default();
    let unique_artists = stats_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let unique_tracks = stats_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);

    Ok(Json(json!({
        "year": year,
        "total_listens": total_listens,
        "total_hours": total_hours,
        "unique_artists": unique_artists,
        "unique_tracks": unique_tracks,
        "max_streak_days": max_streak,
        "top_artists": top_artists,
        "top_tracks": top_tracks,
    })))
}
