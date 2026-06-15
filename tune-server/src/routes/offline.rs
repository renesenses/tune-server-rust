use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
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
        .backend
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

async fn offline_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    ensure_offline_table(&state);
    let total: i64 = state
        .backend
        .query_one("SELECT COUNT(*) FROM offline_cache", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let completed: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM offline_cache WHERE status = 'completed'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let pending: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM offline_cache WHERE status = 'pending' OR status = 'downloading'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let total_size: i64 = state
        .backend
        .query_one(
            "SELECT COALESCE(SUM(file_size), 0) FROM offline_cache WHERE status = 'completed'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cache_dir = offline_cache_dir(&settings);

    Ok(Json(json!({
        "total_tracks": total,
        "completed": completed,
        "pending": pending,
        "total_size_bytes": total_size,
        "total_size_mb": (total_size as f64 / 1_048_576.0 * 100.0).round() / 100.0,
        "cache_dir": cache_dir,
    })))
}

async fn offline_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    use tune_core::db::backend::ToSqlValue;
    let result = state.backend.execute(
        "INSERT INTO offline_cache (source, source_id, quality, status) VALUES (?, ?, ?, 'pending') \
         ON CONFLICT(source, source_id) DO UPDATE SET status = 'pending', quality = excluded.quality",
        &[&source as &dyn ToSqlValue, &source_id as &dyn ToSqlValue, &quality as &dyn ToSqlValue],
    );

    if let Err(e) = result {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    let id = state.backend.last_insert_rowid();

    // Spawn background download task
    let backend = state.backend.clone();
    let services = state.services.clone();
    let http_client = state.http_client.clone();
    let source_owned = source.to_string();
    let source_id_owned = source_id.to_string();
    let quality_owned = quality.to_string();

    tokio::spawn(async move {
        let settings = SettingsRepo::with_backend(backend.clone());
        let cache_dir = offline_cache_dir(&settings);

        // Create cache directory
        std::fs::create_dir_all(format!("{cache_dir}/{source_owned}")).ok();

        // Update status to downloading
        backend
            .execute(
                "UPDATE offline_cache SET status = 'downloading' WHERE id = ?",
                &[&id as &dyn ToSqlValue],
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
                match http_client
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(300))
                    .send()
                    .await
                {
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
                                    backend.execute(
                                        "UPDATE offline_cache SET status = 'completed', file_path = ?, file_size = ?, downloaded_at = CURRENT_TIMESTAMP WHERE id = ?",
                                        &[&file_path as &dyn ToSqlValue, &file_size as &dyn ToSqlValue, &id as &dyn ToSqlValue],
                                    ).ok();
                                    tracing::info!(id, source = %source_owned, source_id = %source_id_owned, "offline_download_complete");
                                } else {
                                    backend.execute(
                                        "UPDATE offline_cache SET status = 'error', error = 'write failed' WHERE id = ?",
                                        &[&id as &dyn ToSqlValue],
                                    ).ok();
                                }
                            }
                            Err(e) => {
                                let err_msg = format!("download body failed: {e}");
                                backend.execute(
                                    "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                                    &[&err_msg as &dyn ToSqlValue, &id as &dyn ToSqlValue],
                                ).ok();
                            }
                        }
                    }
                    Ok(resp) => {
                        let err_msg = format!("HTTP {}", resp.status());
                        backend
                            .execute(
                                "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                                &[&err_msg as &dyn ToSqlValue, &id as &dyn ToSqlValue],
                            )
                            .ok();
                    }
                    Err(e) => {
                        let err_msg = format!("request failed: {e}");
                        backend
                            .execute(
                                "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                                &[&err_msg as &dyn ToSqlValue, &id as &dyn ToSqlValue],
                            )
                            .ok();
                    }
                }
            }
            Err(e) => {
                let err_msg = e.to_string();
                backend
                    .execute(
                        "UPDATE offline_cache SET status = 'error', error = ? WHERE id = ?",
                        &[&err_msg as &dyn ToSqlValue, &id as &dyn ToSqlValue],
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
                    {
                        use tune_core::db::backend::ToSqlValue;
                        state.backend.execute(
                            "INSERT OR IGNORE INTO offline_cache (source, source_id, track_title, artist_name, album_title, quality, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                            &[
                                &source as &dyn ToSqlValue,
                                &track_id as &dyn ToSqlValue,
                                &track["title"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &track["artist"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &track["album"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &quality as &dyn ToSqlValue,
                            ],
                        ).ok();
                    }
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
                    {
                        use tune_core::db::backend::ToSqlValue;
                        state.backend.execute(
                            "INSERT OR IGNORE INTO offline_cache (source, source_id, track_title, artist_name, album_title, quality, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                            &[
                                &source as &dyn ToSqlValue,
                                &track_id as &dyn ToSqlValue,
                                &track["title"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &track["artist"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &track["album"].as_str().unwrap_or("") as &dyn ToSqlValue,
                                &quality as &dyn ToSqlValue,
                            ],
                        ).ok();
                    }
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

async fn list_downloads(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    ensure_offline_table(&state);
    let rows = state.backend.query_many(
        "SELECT id, source, source_id, track_title, artist_name, album_title, file_size, quality, status, error, downloaded_at \
         FROM offline_cache ORDER BY downloaded_at DESC LIMIT 200",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "source": r.get(1).and_then(|v| v.as_string()),
                "source_id": r.get(2).and_then(|v| v.as_string()),
                "track_title": r.get(3).and_then(|v| v.as_string()),
                "artist_name": r.get(4).and_then(|v| v.as_string()),
                "album_title": r.get(5).and_then(|v| v.as_string()),
                "file_size": r.get(6).and_then(|v| v.as_i64()),
                "quality": r.get(7).and_then(|v| v.as_string()),
                "status": r.get(8).and_then(|v| v.as_string()),
                "error": r.get(9).and_then(|v| v.as_string()),
                "downloaded_at": r.get(10).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

async fn download_status(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    ensure_offline_table(&state);
    let row = state.backend.query_one(
        "SELECT id, source, source_id, track_title, artist_name, album_title, file_path, file_size, quality, status, error, downloaded_at, expires_at \
         FROM offline_cache WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    ).map_err(|e| AppError::internal(e))?;

    match row {
        Some(r) => Ok(Json(json!({
            "id": r.get(0).and_then(|v| v.as_i64()),
            "source": r.get(1).and_then(|v| v.as_string()),
            "source_id": r.get(2).and_then(|v| v.as_string()),
            "track_title": r.get(3).and_then(|v| v.as_string()),
            "artist_name": r.get(4).and_then(|v| v.as_string()),
            "album_title": r.get(5).and_then(|v| v.as_string()),
            "file_path": r.get(6).and_then(|v| v.as_string()),
            "file_size": r.get(7).and_then(|v| v.as_i64()),
            "quality": r.get(8).and_then(|v| v.as_string()),
            "status": r.get(9).and_then(|v| v.as_string()),
            "error": r.get(10).and_then(|v| v.as_string()),
            "downloaded_at": r.get(11).and_then(|v| v.as_string()),
            "expires_at": r.get(12).and_then(|v| v.as_string()),
        }))
        .into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_download(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    ensure_offline_table(&state);

    let file_path: Option<String> = state
        .backend
        .query_one(
            "SELECT file_path FROM offline_cache WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()));

    if let Some(ref path) = file_path {
        std::fs::remove_file(path).ok();
    }

    state
        .backend
        .execute(
            "DELETE FROM offline_cache WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn list_offline_albums(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    ensure_offline_table(&state);
    let rows = state.backend.query_many(
        "SELECT album_title, artist_name, COUNT(*) as track_count, COALESCE(SUM(file_size), 0), source \
         FROM offline_cache \
         WHERE status = 'completed' AND album_title IS NOT NULL \
         GROUP BY album_title, artist_name, source \
         ORDER BY album_title",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "album_title": r.get(0).and_then(|v| v.as_string()),
                "artist_name": r.get(1).and_then(|v| v.as_string()),
                "track_count": r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                "total_size": r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
                "source": r.get(4).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!({"albums": items, "total": items.len()})))
}

async fn list_offline_tracks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    ensure_offline_table(&state);
    let rows = state.backend.query_many(
        "SELECT id, source, source_id, track_title, artist_name, album_title, file_path, file_size, duration_ms, quality, downloaded_at \
         FROM offline_cache \
         WHERE status = 'completed' \
         ORDER BY track_title",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "source": r.get(1).and_then(|v| v.as_string()),
                "source_id": r.get(2).and_then(|v| v.as_string()),
                "track_title": r.get(3).and_then(|v| v.as_string()),
                "artist_name": r.get(4).and_then(|v| v.as_string()),
                "album_title": r.get(5).and_then(|v| v.as_string()),
                "file_path": r.get(6).and_then(|v| v.as_string()),
                "file_size": r.get(7).and_then(|v| v.as_i64()),
                "duration_ms": r.get(8).and_then(|v| v.as_i64()),
                "quality": r.get(9).and_then(|v| v.as_string()),
                "downloaded_at": r.get(10).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!({"tracks": items, "total": items.len()})))
}

async fn sync_offline(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    ensure_offline_table(&state);

    let completed: Vec<(i64, String)> = state.backend
        .query_many("SELECT id, file_path FROM offline_cache WHERE status = 'completed' AND file_path IS NOT NULL", &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let id = r.get(0).and_then(|v| v.as_i64())?;
            let path = r.get(1).and_then(|v| v.as_string())?;
            Some((id, path))
        })
        .collect();

    let mut cleaned = 0i64;
    for (id, path) in &completed {
        if !std::path::Path::new(path).exists() {
            state.backend.execute(
                "UPDATE offline_cache SET status = 'missing', file_path = NULL, file_size = NULL WHERE id = ?",
                &[id as &dyn ToSqlValue],
            ).ok();
            cleaned += 1;
        }
    }

    let retried = state
        .backend
        .execute(
            "UPDATE offline_cache SET status = 'pending', error = NULL WHERE status = 'error'",
            &[],
        )
        .unwrap_or(0) as i64;

    Ok(Json(json!({
        "synced": true,
        "missing_cleaned": cleaned,
        "errors_retried": retried,
    })))
}

async fn clear_offline(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    ensure_offline_table(&state);

    let paths: Vec<String> = state
        .backend
        .query_many(
            "SELECT file_path FROM offline_cache WHERE file_path IS NOT NULL",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect();

    for path in &paths {
        std::fs::remove_file(path).ok();
    }

    state
        .backend
        .execute_batch("DELETE FROM offline_cache")
        .ok();

    // Try to remove empty cache directory
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cache_dir = offline_cache_dir(&settings);
    std::fs::remove_dir_all(&cache_dir).ok();

    Ok(Json(json!({
        "cleared": true,
        "files_removed": paths.len(),
    })))
}

// Placeholder type alias for response
use axum::response::Response;
