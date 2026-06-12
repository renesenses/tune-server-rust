use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use lofty::file::TaggedFileExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::track_repo::TrackRepo;

use super::Pagination;

/// Build a JSON array string for the `genres` column from parsed metadata.
fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre {
        if g.is_empty() {
            None
        } else {
            let split = tune_core::metadata::split_genre_tag(g);
            if split.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&split).unwrap_or_default())
            }
        }
    } else {
        None
    }
}

/// Apply freshly-read metadata from disk onto an existing Track struct.
fn apply_metadata_to_track(
    track: &mut tune_core::db::models::Track,
    m: &tune_core::metadata::TrackMetadata,
) {
    if let Some(ref v) = m.title {
        track.title = v.clone();
    }
    if let Some(ref v) = m.artist {
        track.artist_name = Some(v.clone());
    }
    track.album_artist = m.album_artist.clone();
    track.genre = m.genre.clone();
    track.genres = build_genres_json(&m.genres, m.genre.as_deref());
    track.composer = m
        .credits
        .iter()
        .find(|c| c.role == "composer")
        .map(|c| c.name.clone());
    track.year = m.year.map(|y| y as i32);
    track.bpm = m.bpm;
    track.label = m.label.clone();
    track.isrc = m.isrc.clone();
    track.musicbrainz_recording_id = m.musicbrainz_recording_id.clone();
    track.sample_rate = m.sample_rate.map(|s| s as i32);
    track.bit_depth = m.bit_depth.map(|b| b as i32);
    track.channels = m.channels.unwrap_or(2) as i32;
    track.duration_ms = m.duration_ms.unwrap_or(0) as i64;
    track.format = m.format.clone();
    track.track_number = m.track_number.unwrap_or(0) as i32;
    track.disc_number = m.disc_number.unwrap_or(1) as i32;
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

pub(super) async fn list_tracks(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = TrackRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn track_count(State(state): State<AppState>) -> Json<Value> {
    let count = TrackRepo::new(state.db).count().unwrap_or(0);
    Json(json!({ "count": count }))
}

pub(super) async fn get_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(track)) => Json(json!(track)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn stream_track_audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    _req_headers: HeaderMap,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let path = std::path::Path::new(file_path);
    let file_size = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let mime = track
        .format
        .as_deref()
        .and_then(tune_core::audio::formats::AudioFormat::from_extension)
        .map(|f| f.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

    let path_owned = file_path.clone();
    let body = Body::from_stream(async_stream::stream! {
        if let Ok(mut file) = tokio::fs::File::open(&path_owned).await {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 65536];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])),
                    Err(_e) => { break; }
                }
            }
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

pub(super) async fn rescan_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let mut track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return (StatusCode::BAD_REQUEST, "no file path").into_response();
    };

    let meta = tune_core::metadata::read_metadata(std::path::Path::new(file_path));
    match meta {
        Some(m) => {
            apply_metadata_to_track(&mut track, &m);

            if let Err(e) = repo.update(&track) {
                tracing::warn!(track_id = id, error = %e, "rescan_track_update_failed");
            }

            Json(json!({
                "status": "ok",
                "track_id": id,
                "title": m.title,
                "artist": m.artist,
                "album": m.album,
                "genre": m.genre,
                "genres": m.genres,
                "sample_rate": m.sample_rate,
                "bit_depth": m.bit_depth,
                "duration_ms": m.duration_ms,
                "year": m.year,
            }))
            .into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read metadata").into_response(),
    }
}

pub(super) async fn quick_fav_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or(1);
    let repo = ProfileRepo::new(state.db);
    let is_fav = repo.is_favorite(profile_id, "track", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "track", id).ok();
    } else {
        repo.add_favorite(profile_id, "track", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "track_id": id}))
}

pub(super) async fn track_all_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut result = serde_json::to_value(&track).unwrap_or_default();

    // Try reading raw file tags with lofty
    if let Some(ref path) = track.file_path {
        if let Ok(tagged) = lofty::read_from_path(path) {
            let tags: Vec<Value> = tagged
                .tags()
                .iter()
                .map(|tag| {
                    json!({
                        "tag_type": format!("{:?}", tag.tag_type()),
                        "items": tag.items().map(|item| format!("{:?}", item)).collect::<Vec<_>>(),
                    })
                })
                .collect();
            result["file_tags"] = json!(tags);
        }
    }

    Json(result).into_response()
}

pub(super) async fn track_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let genius_token = settings.get("genius_api_token").ok().flatten();
    let Some(token) = genius_token else {
        return Json(json!({"track_id": id, "lyrics": null, "error": "Genius API not configured"}))
            .into_response();
    };
    let title = &track.title;
    let artist = track.artist_name.as_deref().unwrap_or("");
    let q = format!("{title} {artist}");
    let search_url = format!(
        "https://api.genius.com/search?q={}",
        urlencoding::encode(&q)
    );
    let resp = state
        .http_client
        .get(&search_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let data: Value = r.json().await.unwrap_or(json!({}));
            let hits = data.pointer("/response/hits").and_then(|v| v.as_array());
            let url = hits
                .and_then(|arr| arr.first())
                .and_then(|h| h.pointer("/result/url"))
                .and_then(|v| v.as_str());
            Json(json!({
                "track_id": id,
                "title": title,
                "artist": artist,
                "genius_url": url,
                "lyrics": null,
            }))
            .into_response()
        }
        _ => Json(json!({"track_id": id, "lyrics": null, "error": "Genius API request failed"}))
            .into_response(),
    }
}

pub(super) async fn track_synced_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());

    // Check DB cache
    if let Ok(Some(cached)) = repo.get_synced_lyrics(id) {
        let lines: Value = serde_json::from_str(&cached).unwrap_or(Value::Null);
        return Json(json!({ "track_id": id, "synced": true, "lines": lines })).into_response();
    }

    // Try sidecar .lrc file
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return (StatusCode::NOT_FOUND, "track not found").into_response(),
    };

    if let Some(ref path) = track.file_path {
        if let Some(lrc_content) = tune_core::metadata::lyrics::find_sidecar_lrc(path) {
            let lines = tune_core::metadata::lyrics::parse_lrc(&lrc_content);
            if !lines.is_empty() {
                let json_str = serde_json::to_string(&lines).unwrap_or_default();
                repo.set_synced_lyrics(id, &json_str).ok();
                return Json(
                    json!({ "track_id": id, "synced": true, "lines": lines, "source": "lrc_file" }),
                )
                .into_response();
            }
        }
    }

    Json(json!({ "track_id": id, "synced": false, "lines": null })).into_response()
}

pub(super) async fn track_source_links(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = tune_core::db::source_link_repo::SourceLinkRepo::new(state.db);
    let links = repo.get_by_track(id).unwrap_or_default();
    Json(json!({ "track_id": id, "links": links }))
}

pub(super) async fn identify_track(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let api_key = match state.config.acoustid_api_key.as_deref() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "TUNE_ACOUSTID_API_KEY not configured"})),
            )
                .into_response();
        }
    };
    if !tune_core::metadata::fingerprint::fpcalc_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "fpcalc not installed"})),
        )
            .into_response();
    }

    let track_id = match body["track_id"].as_i64() {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "track_id required"})),
            )
                .into_response();
        }
    };

    let repo = TrackRepo::new(state.db.clone());
    let track = match repo.get(track_id) {
        Ok(Some(t)) => t,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response();
        }
    };

    let file_path = match track.file_path.as_deref() {
        Some(p) => p.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "track has no file"})),
            )
                .into_response();
        }
    };

    let fp = match tune_core::metadata::fingerprint::generate_fingerprint(&file_path).await {
        Ok(fp) => fp,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let matches =
        tune_core::metadata::fingerprint::lookup_acoustid(&api_key, &fp.fingerprint, fp.duration)
            .await
            .unwrap_or_default();

    let best = matches.first();
    let confidence = best.map(|m| m.score).unwrap_or(0.0);

    repo.set_acoustid(track_id, &fp.fingerprint, confidence)
        .ok();

    if let Some(m) = best {
        if m.score >= 0.8 && !m.title.is_empty() {
            let conn = state.db.connection().lock().unwrap();
            conn.execute(
                "UPDATE tracks SET title = ?, musicbrainz_recording_id = ? WHERE id = ? AND (title LIKE 'Track %' OR title LIKE 'Unknown%')",
                rusqlite::params![m.title, m.recording_id, track_id],
            ).ok();
            drop(conn);
        }
    }

    Json(json!({
        "track_id": track_id,
        "matched": best.is_some(),
        "confidence": confidence,
        "result": best,
    }))
    .into_response()
}

pub(super) async fn track_waveform(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());

    // Return cached waveform if available
    if let Ok(Some(cached)) = repo.get_waveform(id) {
        return Json(json!({ "track_id": id, "waveform": serde_json::from_str::<Value>(&cached).unwrap_or(Value::Null) })).into_response();
    }

    // Generate on demand
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let file_path = match track.file_path.as_deref() {
        Some(p) => p.to_string(),
        None => {
            return Json(json!({ "track_id": id, "waveform": null, "error": "no file path" }))
                .into_response();
        }
    };

    let points = tune_core::audio::analyzer::generate_waveform(&file_path, 200).await;
    if points.is_empty() {
        return Json(json!({ "track_id": id, "waveform": null, "error": "file unreadable or unsupported format" })).into_response();
    }

    let json_str = serde_json::to_string(&points).unwrap_or_default();
    repo.set_waveform(id, &json_str).ok();

    Json(json!({ "track_id": id, "waveform": points })).into_response()
}

/// POST /api/v1/library/rescan-metadata
///
/// Re-reads tags from audio files for all local tracks and updates the DB.
/// Unlike a full scan, this does NOT discover new files or remove missing ones --
/// it only refreshes metadata (genre, year, artist, etc.) for tracks already in
/// the library. This is what users need after editing tags externally.
pub(super) async fn rescan_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.clone();
    let event_bus = state.event_bus.clone();

    tokio::spawn(async move {
        let db_inner = db.clone();
        let result = tokio::task::spawn_blocking(move || {
            let settings = tune_core::db::settings_repo::SettingsRepo::new(db_inner.clone());
            if let Err(e) = settings.set("rescan_metadata_status", "running") {
                tracing::warn!(error = %e, "rescan_metadata_status_set_failed");
            }

            let track_repo = TrackRepo::new(db_inner.clone());
            let tracks = match track_repo.list_all_local() {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "rescan_metadata_list_failed");
                    settings.set("rescan_metadata_status", "idle").ok();
                    return;
                }
            };

            let total = tracks.len();
            let mut updated = 0usize;
            let mut skipped = 0usize;
            let mut errors = 0usize;

            for track in tracks {
                let Some(ref file_path) = track.file_path else {
                    skipped += 1;
                    continue;
                };

                let path = std::path::Path::new(file_path);
                if !path.exists() {
                    skipped += 1;
                    continue;
                }

                let Some(meta) = tune_core::metadata::read_metadata(path) else {
                    errors += 1;
                    continue;
                };

                let mut t = track.clone();
                apply_metadata_to_track(&mut t, &meta);

                match track_repo.update(&t) {
                    Ok(_) => updated += 1,
                    Err(e) => {
                        tracing::warn!(track_id = ?t.id, error = %e, "rescan_metadata_update_failed");
                        errors += 1;
                    }
                }
            }

            // Refresh album genre/quality from their tracks
            if let Ok(conn) = db_inner.connection().lock() {
                conn.execute_batch(
                    "UPDATE albums SET \
                     genre = (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1), \
                     genres = (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1), \
                     format = (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1), \
                     sample_rate = (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id), \
                     bit_depth = (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id) \
                     WHERE source = 'local' OR source IS NULL",
                )
                .ok();
            }

            settings.set("rescan_metadata_status", "idle").ok();
            settings
                .set(
                    "rescan_metadata_result",
                    &serde_json::json!({
                        "total": total,
                        "updated": updated,
                        "skipped": skipped,
                        "errors": errors,
                    })
                    .to_string(),
                )
                .ok();

            tracing::info!(total, updated, skipped, errors, "rescan_metadata_complete");

            event_bus.emit(
                "library.rescan_metadata.completed",
                serde_json::json!({
                    "total": total,
                    "updated": updated,
                    "skipped": skipped,
                    "errors": errors,
                }),
            );
        })
        .await;

        if let Err(e) = result {
            tracing::error!("rescan_metadata_task_panicked: {:?}", e);
            let settings = tune_core::db::settings_repo::SettingsRepo::new(db);
            settings.set("rescan_metadata_status", "idle").ok();
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "rescan_metadata_started" })),
    )
}

/// GET /api/v1/library/rescan-metadata/status
pub(super) async fn rescan_metadata_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let status = settings
        .get("rescan_metadata_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let result = settings
        .get("rescan_metadata_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    Json(json!({
        "status": status,
        "result": result,
    }))
}
