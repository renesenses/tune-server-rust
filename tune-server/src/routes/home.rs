use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct HomeParams {
    limit: Option<i64>,
    /// Optional zone filter: when provided, continue-listening only shows
    /// albums listened on this zone.  Clients should send the CURRENT active
    /// zone so the response is relevant (DEvir QA B-09: zone mismatch).
    zone_id: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(home_page))
        .route("/continue-listening", get(continue_listening))
        .route("/recently-added", get(recently_added))
        .route("/recommendations", get(home_recommendations))
        .route("/top-mixes", get(top_mixes))
        .route("/new-in-library", get(new_in_library))
        .route("/radio-picks", get(radio_picks))
        .route("/streaming-highlights", get(streaming_highlights))
}

/// Returns a placeholder string appropriate for the engine.
fn ph(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Aggregated home page: returns all sections in a single response.
async fn home_page(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // No zone filter for the aggregated home page — show all zones.
    let continue_items = fetch_continue_listening(&state, 10, None)?;
    let recent_items = fetch_recently_added(&state, 20)?;
    let top_tracks = fetch_top_tracks(&state, 20);
    let radios = fetch_radio_picks(&state)?;
    let discover = fetch_recommendations(&state, 20)?;

    let mut sections = Vec::new();

    if !continue_items.is_empty() {
        sections.push(json!({
            "id": "continue",
            "title": "Continuer l'\u{00e9}coute",
            "type": "albums",
            "items": continue_items,
        }));
    }

    if !recent_items.is_empty() {
        sections.push(json!({
            "id": "recent",
            "title": "Ajout\u{00e9}s r\u{00e9}cemment",
            "type": "albums",
            "items": recent_items,
        }));
    }

    if !top_tracks.is_empty() {
        sections.push(json!({
            "id": "top",
            "title": "Les plus \u{00e9}cout\u{00e9}s",
            "type": "tracks",
            "items": top_tracks,
        }));
    }

    if !radios.is_empty() {
        sections.push(json!({
            "id": "radios",
            "title": "Radios favorites",
            "type": "radios",
            "items": radios,
        }));
    }

    if !discover.is_empty() {
        sections.push(json!({
            "id": "discover",
            "title": "\u{00c0} d\u{00e9}couvrir",
            "type": "albums",
            "items": discover,
        }));
    }

    Ok(Json(json!({ "sections": sections })))
}

/// Albums from listen history where the user hasn't finished the album
/// (listened tracks < total tracks).
async fn continue_listening(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(10);
    // When zone_id is provided, filter continue-listening items to albums
    // that were played on that specific zone. This prevents zone mismatch:
    // the client sends the CURRENT active zone, not a stored zone from
    // history (DEvir QA B-09).
    let items = fetch_continue_listening(&state, limit, p.zone_id)?;
    Ok(Json(json!(items)))
}

fn fetch_continue_listening(
    state: &AppState,
    limit: i64,
    zone_id: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    // When a zone_id filter is provided, only show albums that were listened
    // to on that zone.  This ensures the "continue listening" section matches
    // the user's currently selected zone (B-09 fix).
    let zone_filter = match zone_id {
        Some(zid) => format!("AND lh.zone_id = {zid} "),
        None => String::new(),
    };
    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               COUNT(DISTINCT lh.title) as listened_tracks, a.track_count \
        FROM listen_history lh \
        JOIN albums a ON lh.album_title = a.title \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE a.track_count IS NOT NULL AND a.track_count > 0 \
        {zone_filter}\
        GROUP BY a.id \
        HAVING listened_tracks < a.track_count \
        ORDER BY MAX(lh.listened_at) DESC \
        LIMIT {p1}"
    );
    let params: [&dyn ToSqlValue; 1] = [&limit];
    let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            let album_id = cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            json!({
                "id": album_id,
                "album_id": album_id,
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "album_title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "listened_tracks": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "track_count": cols.get(7).and_then(|v| v.as_i64()),
            })
        })
        .collect())
}

/// Albums added in the last 7 days (by file mtime of tracks).
async fn recently_added(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let items = fetch_recently_added(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_recently_added(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    let seven_days_ago = chrono_epoch_seven_days_ago();
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let sql = format!(
        "SELECT DISTINCT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               a.format, a.sample_rate, a.bit_depth, a.track_count, \
               MAX(t.file_mtime) as newest_mtime \
        FROM tracks t \
        JOIN albums a ON t.album_id = a.id \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL AND t.file_mtime > {p1} \
        GROUP BY a.id \
        ORDER BY newest_mtime DESC \
        LIMIT {p2}"
    );
    let params: [&dyn ToSqlValue; 2] = [&seven_days_ago, &limit];
    let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "format": cols.get(6).and_then(|v| v.as_string()),
                "sample_rate": cols.get(7).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(8).and_then(|v| v.as_i64()),
                "track_count": cols.get(9).and_then(|v| v.as_i64()),
                "added_mtime": cols.get(10).and_then(|v| v.as_f64()),
            })
        })
        .collect())
}

/// Returns epoch seconds for 7 days ago.
fn chrono_epoch_seven_days_ago() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    now - (7.0 * 24.0 * 3600.0)
}

/// Recommendations based on listening history: find most-played genres/artists,
/// suggest albums from the same genres that haven't been listened to yet.
async fn home_recommendations(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let items = fetch_recommendations(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_recommendations(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();

    // Find top genres from listen history
    let top_genres: Vec<String> = state
        .backend
        .query_many(
            "SELECT genre, COUNT(*) as cnt \
             FROM (SELECT COALESCE(t.genre, a.genre) as genre \
                   FROM listen_history lh \
                   LEFT JOIN tracks t ON lh.track_id = t.id \
                   LEFT JOIN albums a ON lh.album_title = a.title \
                   WHERE genre IS NOT NULL AND genre != '') \
             GROUP BY genre ORDER BY cnt DESC LIMIT 5",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_string()))
        .collect();

    if top_genres.is_empty() {
        // Fallback: return random albums
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
                   FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                   ORDER BY RANDOM() LIMIT {p1}"
        );
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
        return Ok(rows
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "year": cols.get(3).and_then(|v| v.as_i64()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "reason": "random",
                })
            })
            .collect());
    }

    // Find albums matching top genres that the user hasn't listened to.
    // Build engine-specific placeholders for the IN clause.
    let genre_placeholders: String = top_genres
        .iter()
        .enumerate()
        .map(|(i, _)| ph(engine, i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let limit_ph = ph(engine, top_genres.len() + 1);
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.genre IN ({genre_placeholders}) \
           AND a.title NOT IN (SELECT DISTINCT album_title FROM listen_history WHERE album_title IS NOT NULL) \
         ORDER BY RANDOM() \
         LIMIT {limit_ph}"
    );

    // Build a Vec of owned SqlValue-able params: genres + limit.
    let mut param_vals: Vec<Box<dyn ToSqlValue>> = top_genres
        .iter()
        .map(|g| Box::new(g.clone()) as Box<dyn ToSqlValue>)
        .collect();
    param_vals.push(Box::new(limit));
    let param_refs: Vec<&dyn ToSqlValue> = param_vals.iter().map(|p| p.as_ref()).collect();

    let rows = state
        .backend
        .query_many(&sql, &param_refs)
        .unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "reason": "genre_match",
            })
        })
        .collect())
}

/// Auto-generated "mixes" by genre from top genres in history.
/// Each mix = playlist of 20 tracks from that genre.
async fn top_mixes(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let engine = state.backend.engine();

    // Get top 5 genres from history
    let top_genres: Vec<(String, i64)> = state
        .backend
        .query_many(
            "SELECT genre, COUNT(*) as cnt \
             FROM (SELECT COALESCE(t.genre, a.genre) as genre \
                   FROM listen_history lh \
                   LEFT JOIN tracks t ON lh.track_id = t.id \
                   LEFT JOIN albums a ON lh.album_title = a.title \
                   WHERE genre IS NOT NULL AND genre != '') \
             GROUP BY genre ORDER BY cnt DESC LIMIT 5",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| {
            let genre = cols.first()?.as_string()?;
            let cnt = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            Some((genre, cnt))
        })
        .collect();

    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let tracks_sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, \
                CAST(t.duration_ms AS BIGINT), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE t.genre = {p1} OR al.genre = {p2} \
         ORDER BY RANDOM() LIMIT 20"
    );

    let mixes: Vec<Value> = top_genres
        .into_iter()
        .filter_map(|(genre, play_count)| {
            let params: [&dyn ToSqlValue; 2] = [&genre, &genre];
            let tracks: Vec<Value> = state
                .backend
                .query_many(&tracks_sql, &params)
                .unwrap_or_default()
                .iter()
                .map(|cols| {
                    json!({
                        "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                        "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                        "artist_name": cols.get(2).and_then(|v| v.as_string()),
                        "album_title": cols.get(3).and_then(|v| v.as_string()),
                        "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                        "cover_path": cols.get(5).and_then(|v| v.as_string()),
                    })
                })
                .collect();

            if tracks.is_empty() {
                return None;
            }

            Some(json!({
                "genre": genre,
                "title": format!("Mix {}", genre),
                "play_count": play_count,
                "track_count": tracks.len(),
                "tracks": tracks,
            }))
        })
        .collect();

    Ok(Json(json!(mixes)))
}

/// Tracks added in the last scan (newest by file_mtime, recent imports).
async fn new_in_library(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(30);
    let engine = state.backend.engine();
    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, \
                CAST(t.duration_ms AS BIGINT), al.cover_path, \
               t.format, t.sample_rate, t.bit_depth, t.file_mtime \
        FROM tracks t \
        LEFT JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON t.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL \
        ORDER BY t.file_mtime DESC \
        LIMIT {p1}"
    );
    let params: [&dyn ToSqlValue; 1] = [&limit];
    let items: Vec<Value> = state
        .backend
        .query_many(&sql, &params)
        .unwrap_or_default()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "cover_path": cols.get(5).and_then(|v| v.as_string()),
                "format": cols.get(6).and_then(|v| v.as_string()),
                "sample_rate": cols.get(7).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(8).and_then(|v| v.as_i64()),
                "file_mtime": cols.get(9).and_then(|v| v.as_f64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Favorite radios + recently played radios.
async fn radio_picks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let items = fetch_radio_picks(&state)?;
    Ok(Json(json!(items)))
}

fn fetch_radio_picks(state: &AppState) -> Result<Vec<Value>, AppError> {
    let repo = RadioRepo::with_backend(state.backend.clone());

    let mut items: Vec<Value> = repo
        .favorites()
        .unwrap_or_default()
        .into_iter()
        .map(|r| json!(r))
        .collect();

    let recent: Vec<Value> = state
        .backend
        .query_many(
            "SELECT id, name, url, logo_url, genre, last_played, play_count \
             FROM radio_stations \
             WHERE is_favorite = 0 AND last_played IS NOT NULL \
             ORDER BY last_played DESC LIMIT 10",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "name": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "url": cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                "logo_url": cols.get(3).and_then(|v| v.as_string()),
                "genre": cols.get(4).and_then(|v| v.as_string()),
                "last_played": cols.get(5).and_then(|v| v.as_string()),
                "play_count": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "is_favorite": false,
            })
        })
        .collect();

    items.extend(recent);
    Ok(items)
}

fn fetch_top_tracks(state: &AppState, limit: i64) -> Vec<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    repo.top_tracks(limit).unwrap_or_default()
}

/// If Tidal/Qobuz authenticated, fetch their featured/new-releases.
async fn streaming_highlights(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let statuses = registry.status_all().await;
    drop(registry);

    let mut highlights: Vec<Value> = Vec::new();

    for svc_status in &statuses {
        let name = svc_status
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let authenticated = svc_status
            .get("authenticated")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);

        if !authenticated {
            continue;
        }

        match name {
            "tidal" | "qobuz" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                    "new_releases_url": format!("/api/v1/streaming/{}/new-releases", name),
                }));
            }
            "spotify" | "deezer" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                }));
            }
            _ => {}
        }
    }

    // If we have authenticated services, also add settings hint
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let preferred_service = settings
        .get("preferred_streaming_service")
        .ok()
        .flatten()
        .unwrap_or_default();

    Json(json!({
        "services": highlights,
        "preferred_service": preferred_service,
    }))
}
