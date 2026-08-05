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
    track.disc_subtitle = m.disc_subtitle.clone();
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

/// Query parameters for GET /library/tracks — supports pagination + metadata filters.
/// All filters combine with AND logic.
#[derive(Deserialize)]
pub(super) struct TrackFilterQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub source: Option<String>,
    pub label: Option<String>,
    pub composer: Option<String>,
    pub q: Option<String>,
    pub artist: Option<String>,
    pub country: Option<String>,
    pub mood: Option<String>,
    pub source_media: Option<String>,
    /// Oxygen folder facet: absolute directory prefix; matches its whole subtree.
    pub folder: Option<String>,
    /// Oxygen rating facet: album rating 1-5 (profile 1).
    pub rating: Option<i32>,
    /// Oxygen collection facet: manual collection name (resolved to album ids).
    pub collection: Option<String>,
}

pub(super) async fn list_tracks(
    State(state): State<AppState>,
    Query(p): Query<TrackFilterQuery>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);

    let has_filters = p.genre.is_some()
        || p.year.is_some()
        || p.format.is_some()
        || p.sample_rate.is_some()
        || p.bit_depth.is_some()
        || p.source.is_some()
        || p.label.is_some()
        || p.composer.is_some()
        || p.q.is_some()
        || p.artist.is_some()
        || p.country.is_some()
        || p.mood.is_some()
        || p.source_media.is_some()
        || p.folder.as_deref().is_some_and(|s| !s.is_empty())
        || p.rating.is_some()
        || p.collection.as_deref().is_some_and(|s| !s.is_empty());

    // Resolve the collection name → album ids (JSON settings), like the facet
    // endpoint, so /library/tracks?collection=<name> filters to its albums.
    let collection_ids: Option<Vec<i64>> = p
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| super::facets::collection_album_ids(&state, name));

    if has_filters {
        match repo.list_filtered(
            p.genre.as_deref(),
            p.year,
            p.format.as_deref(),
            p.sample_rate,
            p.bit_depth,
            p.source.as_deref(),
            p.label.as_deref(),
            p.composer.as_deref(),
            p.q.as_deref(),
            p.artist.as_deref(),
            p.country.as_deref(),
            p.mood.as_deref(),
            p.source_media.as_deref(),
            p.folder.as_deref(),
            p.rating,
            collection_ids.as_deref(),
            limit,
            offset,
        ) {
            Ok((items, total)) => {
                Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
            }
            Err(e) => {
                tracing::error!(error = %e, "list_tracks_filtered_query_failed");
                Json(json!({"items": [], "total": 0, "limit": limit, "offset": offset}))
            }
        }
    } else {
        let total = repo.count().unwrap_or(0);
        let items = match repo.list(limit, offset) {
            Ok(tracks) => tracks,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    limit,
                    offset,
                    total,
                    "list_tracks_query_failed — stats show {total} tracks but query returned error"
                );
                Vec::new()
            }
        };
        Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
    }
}

pub(super) async fn track_count(State(state): State<AppState>) -> Json<Value> {
    let count = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

#[derive(Deserialize)]
pub(super) struct SimilarParams {
    limit: Option<i64>,
}

/// GET /library/tracks/{id}/similar — acoustically similar tracks ("Plus comme
/// ça", Phase 2). Ranks the library by cosine distance to the seed's CLAP
/// embedding via `acoustic_neighbors`, hydrates the tracks and re-emits them in
/// similarity order with a `similarity` score. Empty (not an error) when the
/// seed has no embedding yet — the audio-embedding pass hasn't covered it, or
/// this build never computed vectors — so the client can fall back gracefully.
pub(super) async fn track_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<SimilarParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200) as usize;
    let neighbors =
        tune_core::audio::embedding_store::acoustic_neighbors(&state.backend, id, limit);
    if neighbors.is_empty() {
        return Json(json!({ "seed_track_id": id, "count": 0, "items": [] }));
    }
    let ids: Vec<i64> = neighbors.iter().map(|(t, _)| *t).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .list_by_ids(&ids)
        .unwrap_or_default();
    let by_id: std::collections::HashMap<i64, &tune_core::db::models::Track> =
        tracks.iter().filter_map(|t| t.id.map(|i| (i, t))).collect();
    // Re-emit in acoustic-rank order (list_by_ids is unordered) with the score.
    let items: Vec<Value> = neighbors
        .iter()
        .filter_map(|(tid, score)| {
            let t = by_id.get(tid)?;
            let mut v = serde_json::to_value(t).ok()?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "similarity".into(),
                    json!((score * 1000.0).round() / 1000.0),
                );
            }
            Some(v)
        })
        .collect();
    Json(json!({ "seed_track_id": id, "count": items.len(), "items": items }))
}

pub(super) async fn get_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
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
    let repo = TrackRepo::with_backend(state.backend.clone());
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
    let repo = TrackRepo::with_backend(state.backend.clone());
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
    profile: crate::routes::active_profile::ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = TrackRepo::with_backend(state.backend.clone());
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

/// GET /api/v1/library/tracks/{id}/lyrics — mode « Grand écran + paroles ».
///
/// Contract (the web client is built against this — do not change):
/// - 200: `{"synced": bool, "source": "lrc"|"tag"|"lrclib",
///          "lines": [{"t_ms": <u64|null>, "text": "..."}]}`
///   (`t_ms` is null when the source is unsynchronized)
/// - 404: `{"error": "no_lyrics"}` when no source has lyrics.
///
/// Resolution cascade:
/// 1. Sidecar `.lrc` / `.LRC` next to the audio file → synced, source "lrc".
/// 2. Embedded tag (USLT/LYRICS via lofty; the scanner also persists it in
///    `track_metadata` under the `lyrics` key). LRC timestamps inside the
///    tag → synced; otherwise raw lines with `t_ms: null` → unsynced.
/// 3. LRCLIB, only when the `lyrics_lrclib_enabled` setting is "true"
///    (cache-first, negatives retried after 14 days).
///
/// Never returns 500 for a track without lyrics; LRCLIB failures degrade
/// to a clean 404. No premium gate: this is a display feature.
pub(super) async fn track_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    fn no_lyrics() -> axum::response::Response {
        (StatusCode::NOT_FOUND, Json(json!({"error": "no_lyrics"}))).into_response()
    }

    fn synced_response(
        source: &str,
        lines: &[tune_core::metadata::lyrics::LrcLine],
    ) -> axum::response::Response {
        let out: Vec<Value> = lines
            .iter()
            .map(|l| json!({"t_ms": l.time_ms, "text": l.text}))
            .collect();
        Json(json!({"synced": true, "source": source, "lines": out})).into_response()
    }

    fn plain_response(source: &str, text: &str) -> Option<axum::response::Response> {
        let out: Vec<Value> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| json!({"t_ms": Value::Null, "text": l}))
            .collect();
        if out.is_empty() {
            return None;
        }
        Some(Json(json!({"synced": false, "source": source, "lines": out})).into_response())
    }

    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return no_lyrics(),
    };

    // 1. Sidecar .lrc / .LRC next to the audio file.
    if let Some(ref path) = track.file_path {
        if let Some(content) = tune_core::metadata::lyrics::find_sidecar_lrc(path) {
            let lines = tune_core::metadata::lyrics::parse_lrc(&content);
            if !lines.is_empty() {
                return synced_response("lrc", &lines);
            }
        }
    }

    // 2. Embedded tag: scanner-persisted `track_metadata['lyrics']` first
    // (no file I/O), then a direct lofty read of the file.
    let meta_repo =
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone());
    let tag_lyrics = meta_repo
        .get_all(id)
        .ok()
        .and_then(|m| m.get("lyrics").cloned())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            track
                .file_path
                .as_deref()
                .and_then(tune_core::metadata::lyrics::read_embedded_lyrics)
        });
    if let Some(text) = tag_lyrics {
        let lines = tune_core::metadata::lyrics::parse_lrc(&text);
        if !lines.is_empty() {
            return synced_response("tag", &lines);
        }
        if let Some(resp) = plain_response("tag", &text) {
            return resp;
        }
    }

    // 3. LRCLIB — opt-in via the generic settings key `lyrics_lrclib_enabled`.
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let lrclib_enabled = settings
        .get("lyrics_lrclib_enabled")
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    if !lrclib_enabled {
        return no_lyrics();
    }

    let artist = track.artist_name.clone().unwrap_or_default();
    if artist.is_empty() || track.title.is_empty() {
        return no_lyrics();
    }

    // Cache first (positive entries never expire; negatives retry after 14 d).
    if let Some(entry) = tune_core::lyrics::load_cache_entry(&state.backend, id) {
        if let Some(lrc) = entry
            .synced_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
            if !lines.is_empty() {
                return synced_response("lrclib", &lines);
            }
        }
        if let Some(plain) = entry
            .plain_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if let Some(resp) = plain_response("lrclib", plain) {
                return resp;
            }
        }
        if entry.negative_still_fresh() {
            return no_lyrics();
        }
    }

    let duration_secs = (track.duration_ms > 0).then_some(track.duration_ms / 1000);
    match tune_core::lyrics::fetch_lrclib_raw(
        &state.http_client,
        &artist,
        &track.title,
        track.album_title.as_deref(),
        duration_secs,
    )
    .await
    {
        Ok(raw) => {
            let raw = raw.unwrap_or_default();
            // Cache both hits and misses (misses are retried after 14 days).
            tune_core::lyrics::store_cache_entry(
                &state.backend,
                id,
                &track.title,
                &artist,
                raw.synced_lyrics.as_deref(),
                raw.plain_lyrics.as_deref(),
            );
            if let Some(lrc) = raw.synced_lyrics.as_deref() {
                let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
                if !lines.is_empty() {
                    return synced_response("lrclib", &lines);
                }
            }
            if let Some(plain) = raw.plain_lyrics.as_deref() {
                if let Some(resp) = plain_response("lrclib", plain) {
                    return resp;
                }
            }
            no_lyrics()
        }
        // Network/protocol failure: clean 404, nothing cached so the next
        // request retries.
        Err(e) => {
            tracing::debug!(track_id = id, error = %e, "lrclib_fetch_failed");
            no_lyrics()
        }
    }
}

pub(super) async fn track_synced_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());

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
    let repo = tune_core::db::source_link_repo::SourceLinkRepo::with_backend(state.backend.clone());
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

    let repo = TrackRepo::with_backend(state.backend.clone());
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
            use tune_core::db::backend::ToSqlValue;
            state.backend.execute(
                "UPDATE tracks SET title = ?, musicbrainz_recording_id = ? WHERE id = ? AND (title LIKE 'Track %' OR title LIKE 'Unknown%')",
                &[&m.title as &dyn ToSqlValue, &m.recording_id as &dyn ToSqlValue, &track_id as &dyn ToSqlValue],
            ).ok();
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
    let repo = TrackRepo::with_backend(state.backend.clone());

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
    let backend = state.backend.clone();
    let event_bus = state.event_bus.clone();

    tokio::spawn(async move {
        let backend_inner = backend.clone();
        let result = tokio::task::spawn_blocking(move || {
            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend_inner.clone());
            if let Err(e) = settings.set("rescan_metadata_status", "running") {
                tracing::warn!(error = %e, "rescan_metadata_status_set_failed");
            }

            let track_repo = TrackRepo::with_backend(backend_inner.clone());
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
            backend_inner.execute_batch(
                "UPDATE albums SET \
                 genre = (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1), \
                 genres = (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1), \
                 format = (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1), \
                 sample_rate = (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id), \
                 bit_depth = (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id) \
                 WHERE source = 'local' OR source IS NULL",
            )
            .ok();

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
            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend);
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
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
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

// --- Track extended metadata endpoints ---

/// GET /api/v1/library/tracks/{id}/metadata
/// Returns all extended metadata key-value pairs for a track.
pub(super) async fn track_metadata_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;

    let repo = TrackMetadataRepo::with_backend(state.backend.clone());
    match repo.get_all(id) {
        Ok(meta) => Json(json!(meta)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// PUT /api/v1/library/tracks/{id}/metadata
/// Batch-sets extended metadata fields from a JSON object body.
/// After saving to DB, also writes tags to the audio file (best-effort).
pub(super) async fn track_metadata_put(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;

    // Verify the track exists and get its file_path
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let file_path = match track_repo.get(id) {
        Ok(Some(track)) => track.file_path,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    // Save to DB (source of truth)
    let repo = TrackMetadataRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.set_batch(id, &body) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Write tags to file (best-effort, don't fail the request)
    let mut file_write_error: Option<String> = None;
    if let Some(ref path) = file_path {
        if let Err(e) = tune_core::metadata::tag_writer::write_metadata_to_file(path, &body).await {
            tracing::warn!(
                track_id = id,
                path = path.as_str(),
                error = e.as_str(),
                "tag_write_to_file_failed"
            );
            file_write_error = Some(e);
        }
    }

    let mut resp = json!({"status": "ok", "fields": body.len()});
    if let Some(err) = file_write_error {
        resp["file_write_warning"] = json!(err);
    }
    Json(resp).into_response()
}
