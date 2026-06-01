use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct HomeParams {
    limit: Option<i64>,
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

/// Aggregated home page: returns all sections in a single response.
async fn home_page(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let continue_items = fetch_continue_listening(&state, 10)?;
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
    let items = fetch_continue_listening(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_continue_listening(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let sql = "\
        SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               COUNT(DISTINCT lh.title) as listened_tracks, a.track_count \
        FROM listen_history lh \
        JOIN albums a ON lh.album_title = a.title \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE a.track_count IS NOT NULL AND a.track_count > 0 \
        GROUP BY a.id \
        HAVING listened_tracks < a.track_count \
        ORDER BY MAX(lh.listened_at) DESC \
        LIMIT ?";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(stmt
        .query_map(params![limit], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0).unwrap_or(0),
                "title": row.get::<_, String>(1).unwrap_or_default(),
                "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                "year": row.get::<_, Option<i32>>(3).unwrap_or(None),
                "cover_path": row.get::<_, Option<String>>(4).unwrap_or(None),
                "genre": row.get::<_, Option<String>>(5).unwrap_or(None),
                "listened_tracks": row.get::<_, i64>(6).unwrap_or(0),
                "track_count": row.get::<_, Option<i32>>(7).unwrap_or(None),
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default())
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
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let seven_days_ago = chrono_epoch_seven_days_ago();
    let sql = "\
        SELECT DISTINCT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               a.format, a.sample_rate, a.bit_depth, a.track_count, \
               MAX(t.file_mtime) as newest_mtime \
        FROM tracks t \
        JOIN albums a ON t.album_id = a.id \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL AND t.file_mtime > ? \
        GROUP BY a.id \
        ORDER BY newest_mtime DESC \
        LIMIT ?";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(stmt
        .query_map(params![seven_days_ago, limit], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0).unwrap_or(0),
                "title": row.get::<_, String>(1).unwrap_or_default(),
                "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                "year": row.get::<_, Option<i32>>(3).unwrap_or(None),
                "cover_path": row.get::<_, Option<String>>(4).unwrap_or(None),
                "genre": row.get::<_, Option<String>>(5).unwrap_or(None),
                "format": row.get::<_, Option<String>>(6).unwrap_or(None),
                "sample_rate": row.get::<_, Option<i32>>(7).unwrap_or(None),
                "bit_depth": row.get::<_, Option<i32>>(8).unwrap_or(None),
                "track_count": row.get::<_, Option<i32>>(9).unwrap_or(None),
                "added_mtime": row.get::<_, Option<f64>>(10).unwrap_or(None),
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default())
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
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;

    // Find top genres from listen history
    let top_genres: Vec<String> = conn
        .prepare(
            "SELECT genre, COUNT(*) as cnt \
             FROM (SELECT COALESCE(t.genre, a.genre) as genre \
                   FROM listen_history lh \
                   LEFT JOIN tracks t ON lh.track_id = t.id \
                   LEFT JOIN albums a ON lh.album_title = a.title \
                   WHERE genre IS NOT NULL AND genre != '') \
             GROUP BY genre ORDER BY cnt DESC LIMIT 5",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if top_genres.is_empty() {
        // Fallback: return random albums
        let sql = "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
                   FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                   ORDER BY RANDOM() LIMIT ?";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        return Ok(stmt
            .query_map(params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0).unwrap_or(0),
                    "title": row.get::<_, String>(1).unwrap_or_default(),
                    "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                    "year": row.get::<_, Option<i32>>(3).unwrap_or(None),
                    "cover_path": row.get::<_, Option<String>>(4).unwrap_or(None),
                    "genre": row.get::<_, Option<String>>(5).unwrap_or(None),
                    "reason": "random",
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default());
    }

    // Find albums matching top genres that the user hasn't listened to
    let placeholders: String = top_genres.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.genre IN ({placeholders}) \
           AND a.title NOT IN (SELECT DISTINCT album_title FROM listen_history WHERE album_title IS NOT NULL) \
         ORDER BY RANDOM() \
         LIMIT ?"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = top_genres
        .into_iter()
        .map(|g| Box::new(g) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    param_values.push(Box::new(limit));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();

    Ok(stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0).unwrap_or(0),
                "title": row.get::<_, String>(1).unwrap_or_default(),
                "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                "year": row.get::<_, Option<i32>>(3).unwrap_or(None),
                "cover_path": row.get::<_, Option<String>>(4).unwrap_or(None),
                "genre": row.get::<_, Option<String>>(5).unwrap_or(None),
                "reason": "genre_match",
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default())
}

/// Auto-generated "mixes" by genre from top genres in history.
/// Each mix = playlist of 20 tracks from that genre.
async fn top_mixes(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;

    // Get top 5 genres from history
    let top_genres: Vec<(String, i64)> = conn
        .prepare(
            "SELECT genre, COUNT(*) as cnt \
             FROM (SELECT COALESCE(t.genre, a.genre) as genre \
                   FROM listen_history lh \
                   LEFT JOIN tracks t ON lh.track_id = t.id \
                   LEFT JOIN albums a ON lh.album_title = a.title \
                   WHERE genre IS NOT NULL AND genre != '') \
             GROUP BY genre ORDER BY cnt DESC LIMIT 5",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, i64>(1).unwrap_or(0),
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    let mixes: Vec<Value> = top_genres
        .into_iter()
        .filter_map(|(genre, play_count)| {
            let tracks: Vec<Value> = conn
                .prepare(
                    "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, al.cover_path \
                     FROM tracks t \
                     LEFT JOIN albums al ON t.album_id = al.id \
                     LEFT JOIN artists ar ON t.artist_id = ar.id \
                     WHERE t.genre = ? OR al.genre = ? \
                     ORDER BY RANDOM() LIMIT 20",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map(params![genre, genre], |row| {
                        Ok(json!({
                            "id": row.get::<_, i64>(0).unwrap_or(0),
                            "title": row.get::<_, String>(1).unwrap_or_default(),
                            "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                            "album_title": row.get::<_, Option<String>>(3).unwrap_or(None),
                            "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                            "cover_path": row.get::<_, Option<String>>(5).unwrap_or(None),
                        }))
                    })
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                    .ok()
                })
                .unwrap_or_default();

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
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let sql = "\
        SELECT t.id, t.title, ar.name, al.title, t.duration_ms, al.cover_path, \
               t.format, t.sample_rate, t.bit_depth, t.file_mtime \
        FROM tracks t \
        LEFT JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON t.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL \
        ORDER BY t.file_mtime DESC \
        LIMIT ?";
    let items: Vec<Value> = conn
        .prepare(sql)
        .ok()
        .map(|mut stmt| {
            stmt.query_map(params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0).unwrap_or(0),
                    "title": row.get::<_, String>(1).unwrap_or_default(),
                    "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                    "album_title": row.get::<_, Option<String>>(3).unwrap_or(None),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "cover_path": row.get::<_, Option<String>>(5).unwrap_or(None),
                    "format": row.get::<_, Option<String>>(6).unwrap_or(None),
                    "sample_rate": row.get::<_, Option<i32>>(7).unwrap_or(None),
                    "bit_depth": row.get::<_, Option<i32>>(8).unwrap_or(None),
                    "file_mtime": row.get::<_, Option<f64>>(9).unwrap_or(None),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();
    Ok(Json(json!(items)))
}

/// Favorite radios + recently played radios.
async fn radio_picks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let items = fetch_radio_picks(&state)?;
    Ok(Json(json!(items)))
}

fn fetch_radio_picks(state: &AppState) -> Result<Vec<Value>, AppError> {
    let repo = RadioRepo::new(state.db.clone());

    let mut items: Vec<Value> = repo
        .favorites()
        .unwrap_or_default()
        .into_iter()
        .map(|r| json!(r))
        .collect();

    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let recent: Vec<Value> = conn
        .prepare(
            "SELECT id, name, url, logo_url, genre, last_played, play_count \
             FROM radio_stations \
             WHERE is_favorite = 0 AND last_played IS NOT NULL \
             ORDER BY last_played DESC LIMIT 10",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0).unwrap_or(0),
                    "name": row.get::<_, String>(1).unwrap_or_default(),
                    "url": row.get::<_, String>(2).unwrap_or_default(),
                    "logo_url": row.get::<_, Option<String>>(3).unwrap_or(None),
                    "genre": row.get::<_, Option<String>>(4).unwrap_or(None),
                    "last_played": row.get::<_, Option<String>>(5).unwrap_or(None),
                    "play_count": row.get::<_, i64>(6).unwrap_or(0),
                    "is_favorite": false,
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    items.extend(recent);
    Ok(items)
}

fn fetch_top_tracks(state: &AppState, limit: i64) -> Vec<Value> {
    let repo = HistoryRepo::new(state.db.clone());
    repo.top_tracks(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({
                "title": title,
                "artist_name": artist,
                "plays": plays,
            })
        })
        .collect()
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
    let settings = SettingsRepo::new(state.db.clone());
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
