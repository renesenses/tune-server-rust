use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::license::Feature;

use crate::error::AppError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StatsParams {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct HistoryPeriodParams {
    period: Option<String>,
}

#[derive(Deserialize)]
struct WrappedParams {
    year: Option<i32>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(listening_stats))
        .route("/heatmap", get(heatmap))
        .route("/history", get(history_daily))
        .route("/wrapped", get(wrapped))
}

// ---------------------------------------------------------------------------
// GET /stats/listening — totals + top artists/albums/tracks/genres
// Free: top 3 artists only. Premium: full response.
// ---------------------------------------------------------------------------

async fn listening_stats(
    State(state): State<AppState>,
    Query(p): Query<StatsParams>,
) -> Result<Json<Value>, AppError> {
    let b = &state.backend;
    let limit = p.limit.unwrap_or(10);
    let is_premium = state.license.check_feature(Feature::ListeningStats).await;

    // --- Totals ---
    let totals_row = b
        .query_one(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0), \
             COUNT(DISTINCT COALESCE(title,'') || '::' || COALESCE(artist_name,'')), \
             COUNT(DISTINCT artist_name) \
             FROM listen_history",
            &[],
        )
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();

    let total_listens = totals_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = totals_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let unique_tracks = totals_row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
    let unique_artists = totals_row.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_hours = (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;

    // --- Top artists (always returned, but capped to 3 for free tier) ---
    let artist_limit = if is_premium { limit } else { 3 };
    let top_artists = query_top_artists(b, artist_limit)?;

    // --- Premium-only sections ---
    let (top_albums, top_tracks, top_genres) = if is_premium {
        (
            query_top_albums(b, limit)?,
            query_top_tracks(b, limit)?,
            query_top_genres(b, 20)?,
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(Json(json!({
        "premium": is_premium,
        "totals": {
            "total_listens": total_listens,
            "total_duration_ms": total_ms,
            "total_hours": total_hours,
            "unique_tracks": unique_tracks,
            "unique_artists": unique_artists,
        },
        "top_artists": top_artists,
        "top_albums": top_albums,
        "top_tracks": top_tracks,
        "top_genres": top_genres,
    })))
}

// ---------------------------------------------------------------------------
// GET /stats/listening/heatmap — Premium only
// Returns a 7x24 grid (day_of_week x hour) of play counts.
// ---------------------------------------------------------------------------

async fn heatmap(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::ListeningStats).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "listening_stats",
        })));
    }

    let b = &state.backend;

    let (dow_expr, hour_expr) = match b.engine() {
        Engine::Sqlite => (
            "CAST(strftime('%w', listened_at) AS INTEGER)".to_string(),
            "CAST(strftime('%H', listened_at) AS INTEGER)".to_string(),
        ),
        Engine::Postgres => (
            "EXTRACT(DOW FROM listened_at::timestamp)::int".to_string(),
            "EXTRACT(HOUR FROM listened_at::timestamp)::int".to_string(),
        ),
    };

    let sql = format!(
        "SELECT {dow_expr} as dow, {hour_expr} as hour, COUNT(*) as plays \
         FROM listen_history \
         WHERE listened_at IS NOT NULL \
         GROUP BY dow, hour \
         ORDER BY dow, hour"
    );

    let rows = b.query_many(&sql, &[]).map_err(|e| AppError::internal(e))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "day_of_week": cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                "hour": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(json!({
        "heatmap": items,
    })))
}

// ---------------------------------------------------------------------------
// GET /stats/listening/history?period=week|month|year — Premium only
// Daily play counts for the given period.
// ---------------------------------------------------------------------------

async fn history_daily(
    State(state): State<AppState>,
    Query(p): Query<HistoryPeriodParams>,
) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::ListeningStats).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "listening_stats",
        })));
    }

    let period = p.period.as_deref().unwrap_or("month");
    let days: i64 = match period {
        "week" => 7,
        "year" => 365,
        _ => 30, // month (default)
    };

    let b = &state.backend;

    let day_expr = match b.engine() {
        Engine::Sqlite => SqliteDialect.date_trunc_day("listened_at"),
        Engine::Postgres => PostgresDialect.date_trunc_day("listened_at"),
    };

    let cutoff_expr = match b.engine() {
        Engine::Sqlite => format!("datetime('now', '-{days} days')"),
        Engine::Postgres => format!("now() - interval '{days} days'"),
    };

    let sql = format!(
        "SELECT {day_expr} as day, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as total_ms \
         FROM listen_history \
         WHERE listened_at >= {cutoff_expr} \
         GROUP BY day \
         ORDER BY day"
    );

    let rows = b.query_many(&sql, &[]).map_err(|e| AppError::internal(e))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            let total_ms = cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
            json!({
                "day": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                "total_duration_ms": total_ms,
                "hours": (total_ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
            })
        })
        .collect();

    Ok(Json(json!({
        "period": period,
        "days": days,
        "history": items,
    })))
}

// ---------------------------------------------------------------------------
// GET /stats/listening/wrapped?year=2026 — Premium only
// Annual summary: totals, streaks, top artists/tracks/albums, busiest day.
// ---------------------------------------------------------------------------

async fn wrapped(
    State(state): State<AppState>,
    Query(p): Query<WrappedParams>,
) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::ListeningStats).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "listening_stats",
        })));
    }

    let year = p.year.unwrap_or(2026);
    let year_start = format!("{year}-01-01");
    let year_end = format!("{}-01-01", year + 1);
    let b = &state.backend;

    // --- Totals for the year ---
    let row = b
        .query_one(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0), \
             COUNT(DISTINCT artist_name), \
             COUNT(DISTINCT COALESCE(title,'') || '::' || COALESCE(artist_name,'')) \
             FROM listen_history WHERE listened_at >= ? AND listened_at < ?",
            &[&year_start as &dyn ToSqlValue, &year_end],
        )
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();

    let total_listens = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_hours = (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;
    let unique_artists = row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
    let unique_tracks = row.get(3).and_then(|v| v.as_i64()).unwrap_or(0);

    // --- Top artists for the year ---
    let top_artists: Vec<Value> = b
        .query_many(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? AND artist_name IS NOT NULL \
             GROUP BY artist_name ORDER BY plays DESC LIMIT 10",
            &[&year_start as &dyn ToSqlValue, &year_end],
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

    // --- Top tracks for the year ---
    let top_tracks: Vec<Value> = b
        .query_many(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? \
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT 10",
            &[&year_start as &dyn ToSqlValue, &year_end],
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

    // --- Top albums for the year ---
    let top_albums: Vec<Value> = b
        .query_many(
            "SELECT album_title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? AND album_title IS NOT NULL \
             GROUP BY album_title, artist_name ORDER BY plays DESC LIMIT 10",
            &[&year_start as &dyn ToSqlValue, &year_end],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|cols| {
            json!({
                "album": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    // --- Longest listening streak ---
    let day_expr = match b.engine() {
        Engine::Sqlite => SqliteDialect.date_trunc_day("listened_at"),
        Engine::Postgres => PostgresDialect.date_trunc_day("listened_at"),
    };
    let days_sql = format!(
        "SELECT DISTINCT {day_expr} as d FROM listen_history \
         WHERE listened_at >= ? AND listened_at < ? ORDER BY 1"
    );
    let days: Vec<String> = b
        .query_many(&days_sql, &[&year_start as &dyn ToSqlValue, &year_end])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();

    let max_streak = compute_max_streak(&days);

    // --- Busiest day ---
    let busiest_sql = format!(
        "SELECT {day_expr} as d, COUNT(*) as plays FROM listen_history \
         WHERE listened_at >= ? AND listened_at < ? \
         GROUP BY d ORDER BY plays DESC LIMIT 1"
    );
    let busiest = b
        .query_one(&busiest_sql, &[&year_start as &dyn ToSqlValue, &year_end])
        .ok()
        .flatten();

    let busiest_day = busiest
        .as_ref()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    let busiest_plays = busiest
        .as_ref()
        .and_then(|r| r.get(1).and_then(|v| v.as_i64()))
        .unwrap_or(0);

    Ok(Json(json!({
        "year": year,
        "total_listens": total_listens,
        "total_hours": total_hours,
        "unique_artists": unique_artists,
        "unique_tracks": unique_tracks,
        "max_streak_days": max_streak,
        "busiest_day": busiest_day,
        "busiest_day_plays": busiest_plays,
        "top_artists": top_artists,
        "top_tracks": top_tracks,
        "top_albums": top_albums,
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn query_top_artists(
    b: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    limit: i64,
) -> Result<Vec<Value>, AppError> {
    let rows = b
        .query_many(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history \
             WHERE artist_name IS NOT NULL \
             GROUP BY artist_name ORDER BY plays DESC LIMIT ?",
            &[&limit as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;

    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "artist": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

fn query_top_albums(
    b: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    limit: i64,
) -> Result<Vec<Value>, AppError> {
    let rows = b
        .query_many(
            "SELECT album_title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE album_title IS NOT NULL \
             GROUP BY album_title, artist_name ORDER BY plays DESC LIMIT ?",
            &[&limit as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;

    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "album": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

fn query_top_tracks(
    b: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    limit: i64,
) -> Result<Vec<Value>, AppError> {
    let rows = b
        .query_many(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT ?",
            &[&limit as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;

    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

fn query_top_genres(
    b: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    limit: i64,
) -> Result<Vec<Value>, AppError> {
    // Join listen_history with tracks to pull genre info.
    // Falls back gracefully if no tracks are linked.
    let rows = b
        .query_many(
            "SELECT t.genre, COUNT(*) as plays FROM listen_history lh \
             INNER JOIN tracks t ON t.id = lh.track_id \
             WHERE t.genre IS NOT NULL AND t.genre != '' \
             GROUP BY t.genre ORDER BY plays DESC LIMIT ?",
            &[&limit as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;

    let mut genre_counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for cols in &rows {
        let raw = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
        let plays = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
        for g in tune_core::metadata::split_genre_tag(&raw) {
            *genre_counts.entry(g).or_insert(0) += plays;
        }
    }

    let mut sorted: Vec<(String, i64)> = genre_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(limit as usize);

    Ok(sorted
        .iter()
        .map(|(genre, plays)| json!({ "genre": genre, "plays": plays }))
        .collect())
}

fn compute_max_streak(days: &[String]) -> u32 {
    if days.is_empty() {
        return 0;
    }
    let mut max_streak = 1u32;
    let mut current = 1u32;
    for w in days.windows(2) {
        if is_consecutive_days(&w[0], &w[1]) {
            current += 1;
        } else {
            max_streak = max_streak.max(current);
            current = 1;
        }
    }
    max_streak.max(current)
}

fn is_consecutive_days(a: &str, b: &str) -> bool {
    fn to_days(s: &str) -> Option<i64> {
        // Handle both "YYYY-MM-DD" and "YYYY-MM-DD HH:MM:SS" formats
        let date_part = s.split_whitespace().next().unwrap_or(s);
        let parts: Vec<&str> = date_part.split('-').collect();
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
