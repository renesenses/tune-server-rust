use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct HistoryParams {
    limit: Option<i64>,
    offset: Option<i64>,
    #[allow(dead_code)]
    period: Option<String>,
}

#[derive(Deserialize)]
struct DashboardParams {
    period: Option<String>,
    zone_id: Option<i64>,
    profile_id: Option<i64>,
    top_n: Option<i64>,
}

#[derive(Deserialize)]
struct SlotParams {
    period: Option<String>,
    weekday: i64,
    hour: i64,
    limit: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(recent_history).delete(clear_history))
        .route("/top-tracks", get(top_tracks))
        .route("/tracks/{id}/plays", get(track_plays))
        .route("/top-artists", get(top_artists))
        .route("/top-albums", get(top_albums))
        .route("/dashboard", get(dashboard))
        .route("/at", get(slot_tracks))
        .route("/export", get(export_csv))
}

/// GET /library/history/tracks/{id}/plays — how many times this track was
/// played (non-radio), so the UI can show a play count on a track (Progman,
/// feature #1056). Matched by title + artist, like the dashboard top tracks.
async fn track_plays(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let (title, artist) = match track_repo.get(id) {
        Ok(Some(t)) => (t.title, t.artist_name),
        _ => return Json(json!({ "track_id": id, "plays": 0 })),
    };
    let plays = HistoryRepo::with_backend(state.backend.clone())
        .track_plays(&title, artist.as_deref())
        .unwrap_or(0);
    Json(json!({ "track_id": id, "plays": plays }))
}

/// Tracks listened during one weekday×hour heatmap cell (drill-down).
async fn slot_tracks(State(state): State<AppState>, Query(p): Query<SlotParams>) -> Json<Value> {
    let period = p.period.as_deref().unwrap_or("30d");
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.history_at_slot(period, p.weekday, p.hour, limit) {
        Ok(items) => Json(json!({
            "weekday": p.weekday,
            "hour": p.hour,
            "period": period,
            "tracks": items,
        })),
        Err(e) => {
            tracing::warn!(error = %e, weekday = p.weekday, hour = p.hour, "history_slot_failed");
            Json(json!({ "weekday": p.weekday, "hour": p.hour, "period": period, "tracks": [] }))
        }
    }
}

async fn recent_history(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let (items, total) = repo.recent_paginated(limit, offset).unwrap_or_default();
    Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

async fn clear_history(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.clear() {
        Ok(()) => Json(json!({ "status": "ok" })),
        Err(e) => {
            tracing::error!(error = %e, "clear_history_failed");
            Json(json!({ "status": "error", "detail": e }))
        }
    }
}

async fn top_albums(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = repo
        .top_albums(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({ "album_title": title, "artist_name": artist, "plays": plays })
        })
        .collect();
    Json(json!(items))
}

async fn top_tracks(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.top_tracks(limit).unwrap_or_default();
    Json(json!(items))
}

async fn top_artists(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let engine = state.backend.engine();
    let p1 = if engine == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?"
    };
    let sql = format!(
        "SELECT lh.artist_name, COUNT(*) as plays, ar.id as artist_id \
         FROM listen_history lh \
         LEFT JOIN artists ar ON LOWER(lh.artist_name) = LOWER(ar.name) \
         WHERE lh.artist_name IS NOT NULL \
         GROUP BY lh.artist_name, ar.id \
         ORDER BY plays DESC \
         LIMIT {p1}"
    );
    use tune_core::db::backend::ToSqlValue;
    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "name": cols.get(0).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(0).and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                "artist_id": cols.get(2).and_then(|v| v.as_i64()),
                "id": cols.get(2).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Json(json!(items))
}

async fn dashboard(State(state): State<AppState>, Query(p): Query<DashboardParams>) -> Json<Value> {
    let period = p.period.as_deref().unwrap_or("30d");
    let top_n = p.top_n.unwrap_or(10);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.full_dashboard(period, p.zone_id, p.profile_id, top_n) {
        Ok(data) => Json(json!(data)),
        Err(e) => {
            tracing::error!(error = %e, period, "full_dashboard_failed");
            Json(json!({
                "period": period,
                "range": { "from": null, "to": "" },
                "totals": { "plays": 0, "listening_ms": 0, "unique_tracks": 0, "unique_artists": 0 },
                "top_artists": [],
                "top_albums": [],
                "top_tracks": [],
                "trend": [],
                "hourly": [],
                "by_zone": [],
                "by_source": [],
                "completion": { "completed": 0, "skipped": 0, "avg_listened_ms": 0, "avg_track_duration_ms": 0 }
            }))
        }
    }
}

async fn export_csv(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> impl axum::response::IntoResponse {
    let limit = p.limit.unwrap_or(10000);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let (items, _) = repo.recent_paginated(limit, 0).unwrap_or_default();

    let mut csv = String::from("title,artist,album,source,duration_ms,listened_at,zone_id\n");
    for item in &items {
        let title = item.title.replace(',', ";");
        let artist = item.artist_name.as_deref().unwrap_or("").replace(',', ";");
        let album = item.album_title.as_deref().unwrap_or("").replace(',', ";");
        let source = &item.source;
        let dur = item.duration_ms;
        let listened = item.listened_at.as_deref().unwrap_or("");
        let zone = item.zone_id.unwrap_or(0);
        csv.push_str(&format!(
            "{title},{artist},{album},{source},{dur},{listened},{zone}\n"
        ));
    }

    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"tune-history.csv\"",
            ),
        ],
        csv,
    )
}
