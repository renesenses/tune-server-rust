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
        .route("/status", get(offline_status))
        .route("/config", get(offline_config).post(set_offline_config))
        .route("/download", post(download_for_offline))
        .route("/downloads", get(list_downloads))
        .route(
            "/downloads/{id}",
            get(download_status).delete(delete_download),
        )
        .route("/albums", get(list_offline_albums))
        .route("/tracks", get(list_offline_tracks))
        .route("/sync", post(sync_offline))
        .route("/clear", post(clear_offline))
}

fn ensure_offline_table(state: &AppState) {
    state
        .db
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS offline_cache (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            track_title TEXT,
            artist_name TEXT,
            album_title TEXT,
            file_path TEXT,
            file_size INTEGER,
            duration_ms INTEGER,
            quality TEXT,
            downloaded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            expires_at DATETIME,
            status TEXT DEFAULT 'pending',
            error TEXT,
            UNIQUE(source, source_id)
        )",
        )
        .ok();
}

fn offline_cache_dir(settings: &SettingsRepo) -> String {
    settings
        .get("offline_cache_dir")
        .ok()
        .flatten()
        .unwrap_or_else(|| "offline_cache".into())
}

async fn offline_status(State(state): State<AppState>) -> Json<Value> {
    ensure_offline_table(&state);
    let conn = state.db.connection().lock().unwrap();

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM offline_cache", [], |r| r.get(0))
        .unwrap_or(0);
    let completed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM offline_cache WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM offline_cache WHERE status = 'pending' OR status = 'downloading'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_size: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(file_size), 0) FROM offline_cache WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    let settings = SettingsRepo::new(state.db);
    let cache_dir = offline_cache_dir(&settings);

    Json(json!({
        "total_tracks": total,
        "completed": completed,
        "pending": pending,
        "total_size_bytes": total_size,
        "total_size_mb": (total_size as f64 / 1_048_576.0 * 100.0).round() / 100.0,
        "cache_dir": cache_dir,
    }))
}

async fn offline_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let cache_dir = offline_cache_dir(&settings);
    let max_size_mb: i64 = settings
        .get("offline_max_size_mb")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let auto_sync = settings
        .get("offline_auto_sync")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "cache_dir": cache_dir,
        "max_size_mb": max_size_mb,
        "auto_sync": auto_sync,
    }))
}

#[derive(Deserialize)]
struct OfflineConfigReq {
    cache_dir: Option<String>,
    max_size_mb: Option<i64>,
    auto_sync: Option<bool>,
}

async fn set_offline_config(
    State(state): State<AppState>,
    Json(body): Json<OfflineConfigReq>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    if let Some(ref dir) = body.cache_dir {
        settings.set("offline_cache_dir", dir).ok();
    }
    if let Some(size) = body.max_size_mb {
        settings.set("offline_max_size_mb", &size.to_string()).ok();
    }
    if let Some(sync) = body.auto_sync {
        settings
            .set("offline_auto_sync", if sync { "true" } else { "false" })
            .ok();
    }
    Json(json!({"status": "ok"}))
}

#[derive(Deserialize)]
struct DownloadRequest {
    source: String,
    source_id: String,
    #[serde(rename = "type")]
    download_type: Option<String>,
    quality: Option<String>,
}

async fn download_for_offline(
    State(state): State<AppState>,
    Json(body): Json<DownloadRequest>,
) -> impl IntoResponse {
    ensure_offline_table(&state);

    let download_type = body.download_type.as_deref().unwrap_or("track");
    let quality = body.quality.as_deref().unwrap_or("lossless");

    match download_type {
        "album" => {
            // Queue all tracks from album for download
            download_album_tracks(&state, &body.source, &body.source_id, quality).await
        }
        "playlist" => {
            // Queue all tracks from playlist for download
            download_playlist_tracks(&state, &body.source, &body.source_id, quality).await
        }
        _ => {
            // Single track download
            download_single_track(&state, &body.source, &body.source_id, quality).await
        }
    }
}

async fn download_single_track(
    state: &AppState,
    source: &str,
    source_id: &str,
    quality: &str,
) -> Response {
    // Insert or update the offline_cache entry
    let result = state.db.execute(
        "INSERT INTO offline_cache (source, source_id, quality, status) VALUES (?, ?, ?, 'pending') \
         ON CONFLICT(source, source_id) DO UPDATE SET status = 'pending', quality = excluded.quality",
        &[
            &source as &dyn rusqlite::types::ToSql,
            &source_id,
            &quality,
        ],
    );

    if let Err(e) = result {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    let id = state.db.last_insert_rowid();

    // Spawn background download task
    let db = state.db.clone();
    let services = state.services.clone();
    let source_owned = source.to_string();
    let source_id_owned = source_id.to_string();
    let quality_owned = quality.to_string();

    tokio::spawn(async move {
        let settings = SettingsRepo::new(db.clone());
        let cache_dir = offline_cache_dir(&settings);

        // Create cache directory
        std::fs::create_dir_all(format!("{cache_dir}/{source_owned}")).ok();

        // Update status to downloading
        db.execute(
            "UPDATE offline_cache SET status = 'downloading' WHERE id = ?",
            &[&id as &dyn rusqlite::types::ToSql],
        )
        .ok();

        // Get stream URL from service
        let registry = services.lock().await;
        let stream_result = registry
            .get_stream_url(&source_owned, &source_id_owned, Some(&quality_owned))
            .await;
        drop(registry);

        match stream_result {
            Ok(url) => {
                // Download the file
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(300))
                    .build()
                    .unwrap_or_default();

                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let content_type = resp
                            .headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("audio/flac")
                            .to_string();
                        let ext = match content_type.as_str() {
                            "audio/flac" | "audio/x-flac" => "flac",
                            "audio/mpeg" => "mp3",
                            "audio/ogg" => "ogg",
                            "audio/aac" => "aac",
                            "audio/wav" | "audio/x-wav" => "wav",
                            _ => "flac",
                        };

                        let file_path =
                            format!("{cache_dir}/{source_owned}/{source_id_owned}.{ext}");

                        match resp.bytes().await {
                            Ok(bytes) => {
                                let file_size = bytes.len() as i64;
                                if std::fs::write(&file_path, &bytes).is_ok() {
                                    db.execute(
                                        "UPDATE offline_cache SET status = 'completed', file_path = ?, file_size = ?, downloaded_at = CURRENT_TIMESTAMP WHERE id = ?",
                                        &[&file_path as &dyn rusqlite::types::ToSql, &file_size, &id],
                                    ).ok();
                                    tracing::info!(id, source = %source_owned, source_id = %source_id_owned, "offline_download_complete");
                                } else {
                                    db.execute(
                                        "UPDATE offline_cache SET status = 'error', error = 'write failed' WHERE id = ?",
                                        &[&id as &dyn rusqlite::types::ToSql],
                                    ).ok();
                                }
                            }
                            Err(e) => {
                                let err_msg = format!("download body failed: {e}");
                                db.execute(
                                    "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                                    &[&err_msg as &dyn rusqlite::types::ToSql, &id],
                                )
                                .ok();
                            }
                        }
                    }
                    Ok(resp) => {
                        let err_msg = format!("HTTP {}", resp.status());
                        db.execute(
                            "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                            &[&err_msg as &dyn rusqlite::types::ToSql, &id],
                        )
                        .ok();
                    }
                    Err(e) => {
                        let err_msg = format!("request failed: {e}");
                        db.execute(
                            "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                            &[&err_msg as &dyn rusqlite::types::ToSql, &id],
                        )
                        .ok();
                    }
                }
            }
            Err(e) => {
                db.execute(
                    "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                    &[&e as &dyn rusqlite::types::ToSql, &id],
                )
                .ok();
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": id,
            "status": "pending",
            "source": source,
            "source_id": source_id,
        })),
    )
        .into_response()
}

async fn download_album_tracks(
    state: &AppState,
    source: &str,
    album_id: &str,
    quality: &str,
) -> Response {
    // Get album tracks from service
    let registry = state.services.lock().await;
    let tracks_result = registry.get_album_tracks(source, album_id).await;
    drop(registry);

    match tracks_result {
        Ok(tracks) => {
            let mut queued = 0;
            for track in &tracks {
                let track_id = track["id"].as_str().unwrap_or("");
                if !track_id.is_empty() {
                    state.db.execute(
                        "INSERT OR IGNORE INTO offline_cache (source, source_id, track_title, artist_name, album_title, quality, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                        &[
                            &source as &dyn rusqlite::types::ToSql,
                            &track_id,
                            &track["title"].as_str().unwrap_or(""),
                            &track["artist"].as_str().unwrap_or(""),
                            &track["album"].as_str().unwrap_or(""),
                            &quality,
                        ],
                    ).ok();
                    queued += 1;
                }
            }
            Json(json!({
                "status": "queued",
                "tracks_queued": queued,
                "source": source,
                "album_id": album_id,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("failed to get album tracks: {e}")})),
        )
            .into_response(),
    }
}

async fn download_playlist_tracks(
    state: &AppState,
    source: &str,
    playlist_id: &str,
    quality: &str,
) -> Response {
    // Get playlist tracks from service
    let registry = state.services.lock().await;
    let tracks_result = registry.get_playlist_tracks(source, playlist_id).await;
    drop(registry);

    match tracks_result {
        Ok(tracks) => {
            let mut queued = 0;
            for track in &tracks {
                let track_id = track["id"].as_str().unwrap_or("");
                if !track_id.is_empty() {
                    state.db.execute(
                        "INSERT OR IGNORE INTO offline_cache (source, source_id, track_title, artist_name, album_title, quality, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                        &[
                            &source as &dyn rusqlite::types::ToSql,
                            &track_id,
                            &track["title"].as_str().unwrap_or(""),
                            &track["artist"].as_str().unwrap_or(""),
                            &track["album"].as_str().unwrap_or(""),
                            &quality,
                        ],
                    ).ok();
                    queued += 1;
                }
            }
            Json(json!({
                "status": "queued",
                "tracks_queued": queued,
                "source": source,
                "playlist_id": playlist_id,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("failed to get playlist tracks: {e}")})),
        )
            .into_response(),
    }
}

async fn list_downloads(State(state): State<AppState>) -> Json<Value> {
    ensure_offline_table(&state);
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, source, source_id, track_title, artist_name, album_title, file_size, quality, status, error, downloaded_at \
             FROM offline_cache ORDER BY downloaded_at DESC LIMIT 200",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "source": row.get::<_, Option<String>>(1).ok().flatten(),
                    "source_id": row.get::<_, Option<String>>(2).ok().flatten(),
                    "track_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(4).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(5).ok().flatten(),
                    "file_size": row.get::<_, Option<i64>>(6).ok().flatten(),
                    "quality": row.get::<_, Option<String>>(7).ok().flatten(),
                    "status": row.get::<_, Option<String>>(8).ok().flatten(),
                    "error": row.get::<_, Option<String>>(9).ok().flatten(),
                    "downloaded_at": row.get::<_, Option<String>>(10).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    Json(json!(items))
}

async fn download_status(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    ensure_offline_table(&state);
    let conn = state.db.connection().lock().unwrap();
    let result = conn.query_row(
        "SELECT id, source, source_id, track_title, artist_name, album_title, file_path, file_size, quality, status, error, downloaded_at, expires_at \
         FROM offline_cache WHERE id = ?",
        rusqlite::params![id],
        |row| {
            Ok(json!({
                "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                "source": row.get::<_, Option<String>>(1).ok().flatten(),
                "source_id": row.get::<_, Option<String>>(2).ok().flatten(),
                "track_title": row.get::<_, Option<String>>(3).ok().flatten(),
                "artist_name": row.get::<_, Option<String>>(4).ok().flatten(),
                "album_title": row.get::<_, Option<String>>(5).ok().flatten(),
                "file_path": row.get::<_, Option<String>>(6).ok().flatten(),
                "file_size": row.get::<_, Option<i64>>(7).ok().flatten(),
                "quality": row.get::<_, Option<String>>(8).ok().flatten(),
                "status": row.get::<_, Option<String>>(9).ok().flatten(),
                "error": row.get::<_, Option<String>>(10).ok().flatten(),
                "downloaded_at": row.get::<_, Option<String>>(11).ok().flatten(),
                "expires_at": row.get::<_, Option<String>>(12).ok().flatten(),
            }))
        },
    );
    drop(conn);

    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_download(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    ensure_offline_table(&state);

    // Get file path before deleting record
    let conn = state.db.connection().lock().unwrap();
    let file_path: Option<String> = conn
        .query_row(
            "SELECT file_path FROM offline_cache WHERE id = ?",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    drop(conn);

    // Delete the cached file
    if let Some(ref path) = file_path {
        std::fs::remove_file(path).ok();
    }

    // Delete the DB record
    state
        .db
        .execute(
            "DELETE FROM offline_cache WHERE id = ?",
            &[&id as &dyn rusqlite::types::ToSql],
        )
        .ok();

    StatusCode::NO_CONTENT
}

async fn list_offline_albums(State(state): State<AppState>) -> Json<Value> {
    ensure_offline_table(&state);
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT album_title, artist_name, COUNT(*) as track_count, COALESCE(SUM(file_size), 0), source \
             FROM offline_cache \
             WHERE status = 'completed' AND album_title IS NOT NULL \
             GROUP BY album_title, artist_name, source \
             ORDER BY album_title",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "album_title": row.get::<_, Option<String>>(0).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(1).ok().flatten(),
                    "track_count": row.get::<_, i64>(2).unwrap_or(0),
                    "total_size": row.get::<_, i64>(3).unwrap_or(0),
                    "source": row.get::<_, Option<String>>(4).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    Json(json!({"albums": items, "total": items.len()}))
}

async fn list_offline_tracks(State(state): State<AppState>) -> Json<Value> {
    ensure_offline_table(&state);
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, source, source_id, track_title, artist_name, album_title, file_path, file_size, duration_ms, quality, downloaded_at \
             FROM offline_cache \
             WHERE status = 'completed' \
             ORDER BY track_title",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "source": row.get::<_, Option<String>>(1).ok().flatten(),
                    "source_id": row.get::<_, Option<String>>(2).ok().flatten(),
                    "track_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(4).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(5).ok().flatten(),
                    "file_path": row.get::<_, Option<String>>(6).ok().flatten(),
                    "file_size": row.get::<_, Option<i64>>(7).ok().flatten(),
                    "duration_ms": row.get::<_, Option<i64>>(8).ok().flatten(),
                    "quality": row.get::<_, Option<String>>(9).ok().flatten(),
                    "downloaded_at": row.get::<_, Option<String>>(10).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    Json(json!({"tracks": items, "total": items.len()}))
}

async fn sync_offline(State(state): State<AppState>) -> impl IntoResponse {
    ensure_offline_table(&state);

    // Clean up entries where local file is missing
    let conn = state.db.connection().lock().unwrap();
    let completed: Vec<(i64, String)> = conn
        .prepare("SELECT id, file_path FROM offline_cache WHERE status = 'completed' AND file_path IS NOT NULL")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0),
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    let mut cleaned = 0i64;
    for (id, path) in &completed {
        if !std::path::Path::new(path).exists() {
            state
                .db
                .execute(
                    "UPDATE offline_cache SET status = 'missing', file_path = NULL, file_size = NULL WHERE id = ?",
                    &[id as &dyn rusqlite::types::ToSql],
                )
                .ok();
            cleaned += 1;
        }
    }

    // Reset errored entries to pending for retry
    let retried = state
        .db
        .execute(
            "UPDATE offline_cache SET status = 'pending', error = NULL WHERE status = 'error'",
            &[],
        )
        .unwrap_or(0) as i64;

    Json(json!({
        "synced": true,
        "missing_cleaned": cleaned,
        "errors_retried": retried,
    }))
}

async fn clear_offline(State(state): State<AppState>) -> impl IntoResponse {
    ensure_offline_table(&state);

    // Get all file paths and delete files
    let conn = state.db.connection().lock().unwrap();
    let paths: Vec<String> = conn
        .prepare("SELECT file_path FROM offline_cache WHERE file_path IS NOT NULL")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    for path in &paths {
        std::fs::remove_file(path).ok();
    }

    state.db.execute_batch("DELETE FROM offline_cache").ok();

    // Try to remove empty cache directory
    let settings = SettingsRepo::new(state.db);
    let cache_dir = offline_cache_dir(&settings);
    std::fs::remove_dir_all(&cache_dir).ok();

    Json(json!({
        "cleared": true,
        "files_removed": paths.len(),
    }))
}

// Placeholder type alias for response
use axum::response::Response;
