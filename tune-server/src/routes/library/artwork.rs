use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;

use super::artwork_cache_dir;

fn is_hex_hash(s: &str) -> bool {
    (s.len() == 32 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Deserialize)]
pub(super) struct ProxyQuery {
    url: String,
}

pub(super) async fn serve_artwork(Path(hash): Path<String>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    for ext in &["jpg", "png", "webp"] {
        let path = cache_dir.join(format!("{hash}.{ext}"));
        if path.exists()
            && let Ok(data) = tokio::fs::read(&path).await
        {
            // webp is common for radio station logos and custom radio uploads
            // (set_radio_artwork accepts it) but serve_artwork never served it →
            // 404 → blank cover (Bilou).
            let mime = match *ext {
                "png" => "image/png",
                "webp" => "image/webp",
                _ => "image/jpeg",
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("Content-Type", axum::http::HeaderValue::from_static(mime));
            headers.insert(
                "Cache-Control",
                axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            headers.insert(
                "ETag",
                axum::http::HeaderValue::from_str(&format!("\"{hash}\""))
                    .unwrap_or(axum::http::HeaderValue::from_static("\"artwork\"")),
            );
            return (StatusCode::OK, headers, data).into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

pub(super) async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref cover_path) = album.cover_path {
        if cover_path.starts_with("http") {
            return axum::response::Redirect::temporary(cover_path).into_response();
        }
        let hash = if is_hex_hash(cover_path) {
            cover_path.to_string()
        } else {
            tune_core::library::artwork::artwork_hash(cover_path)
        };
        return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
            .into_response();
    }

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if let Some(track) = tracks.first()
        && let Some(ref file_path) = track.file_path
    {
        let cache_dir = artwork_cache_dir();
        if let Some(hash) =
            tune_core::library::artwork::get_or_extract(std::path::Path::new(file_path), &cache_dir)
        {
            return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
                .into_response();
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

pub(super) async fn upload_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    if album_repo.get(id).ok().flatten().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "album not found"})),
        )
            .into_response();
    }

    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" || name == "artwork" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };

    let cache_dir = artwork_cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    let hash = tune_core::library::artwork::artwork_hash(&format!("album-upload-{id}"));
    let path = cache_dir.join(format!("{hash}.{ext}"));
    if std::fs::write(&path, &data).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    album_repo.force_update_cover_path(id, &hash).ok();

    // Return the updated album
    match album_repo.get(id) {
        Ok(Some(album)) => Json(json!({
            "album": album.to_json(),
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
        _ => Json(json!({
            "album_id": id,
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
    }
}

pub(super) async fn proxy_artwork(
    State(state): State<AppState>,
    Query(q): Query<ProxyQuery>,
) -> impl IntoResponse {
    match state.http_client.get(&q.url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            match resp.bytes().await {
                Ok(data) => {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "Content-Type",
                        HeaderValue::from_str(&content_type)
                            .unwrap_or(HeaderValue::from_static("image/jpeg")),
                    );
                    headers.insert(
                        "Cache-Control",
                        HeaderValue::from_static("public, max-age=86400"),
                    );
                    (StatusCode::OK, headers, data.to_vec()).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub(super) async fn enrich_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album not found"})),
            )
                .into_response();
        }
    };

    // Skip if album already has a non-empty cover
    if album.cover_path.as_ref().is_some_and(|p| !p.is_empty()) {
        return Json(json!({"enriched": false, "reason": "album already has cover art"}))
            .into_response();
    }

    // Step 1: Determine MBID — use existing or search MusicBrainz by artist+title
    let mbid = match album
        .musicbrainz_release_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => {
            let artist = album.artist_name.as_deref().unwrap_or("");
            if !artist.is_empty() && !album.title.is_empty() {
                let found =
                    tune_core::library::artwork::search_musicbrainz_release(artist, &album.title)
                        .await;
                if let Some(ref discovered_mbid) = found {
                    // Store the discovered MBID on the album for future use
                    state.backend.execute(
                        "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                        &[discovered_mbid as &dyn tune_core::db::backend::ToSqlValue, &id as &dyn tune_core::db::backend::ToSqlValue],
                    ).ok();
                    tracing::info!(
                        album_id = id,
                        mbid = %discovered_mbid,
                        album = %album.title,
                        artist = %artist,
                        "enrich_album_artwork_mbid_discovered"
                    );
                }
                found
            } else {
                None
            }
        }
    };

    let Some(ref mbid_val) = mbid else {
        return Json(json!({
            "enriched": false,
            "reason": "no MusicBrainz release ID and could not find one by artist/title"
        }))
        .into_response();
    };

    // Step 2: Fetch cover from Cover Art Archive
    match tune_core::library::artwork::fetch_cover_art(mbid_val).await {
        Some(data) => {
            let cache_dir = artwork_cache_dir();
            let hash = tune_core::library::artwork::artwork_hash(mbid_val);
            if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, "jpg").is_some()
            {
                repo.update_cover_path(id, &hash).ok();
                Json(json!({"enriched": true, "hash": hash, "size": data.len(), "mbid": mbid_val}))
                    .into_response()
            } else {
                Json(json!({"enriched": false, "reason": "failed to save to cache"}))
                    .into_response()
            }
        }
        None => {
            Json(json!({"enriched": false, "reason": "no cover art found on Cover Art Archive"}))
                .into_response()
        }
    }
}

pub(super) async fn batch_enrich_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Check how many albums are missing covers
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let missing = album_repo.list_without_cover().unwrap_or_default();

    if missing.is_empty() {
        return Json(json!({
            "status": "skipped",
            "message": "all albums already have cover art",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artwork_enrich_status", "running").ok();
    settings
        .set(
            "artwork_enrich_result",
            &json!({"total": missing.len(), "enriched": 0, "status": "running"}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artwork enrichment started",
            "albums_to_process": missing.len(),
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artwork_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let still_missing = album_repo.list_without_cover().unwrap_or_default().len();

    Json(json!({
        "result": result,
        "albums_without_cover": still_missing,
    }))
}

pub(super) async fn batch_enrich_artist_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Count artists missing MBIDs (Phase 1 candidates)
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let without_mbid = artist_repo.list_without_mbid().unwrap_or_default().len();

    // Count artists missing images (Phase 2 candidates)
    let missing = artist_repo.list_without_image().unwrap_or_default();

    if missing.is_empty() && without_mbid == 0 {
        return Json(json!({
            "status": "skipped",
            "message": "all artists already have MBID and images",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artist_artwork_enrich_status", "running").ok();
    settings
        .set(
            "artist_artwork_enrich_result",
            &json!({"total": missing.len(), "enriched": 0, "without_mbid": without_mbid, "status": "running"}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        // Phase 1: Match artists without MBID by searching MusicBrainz
        let matched = tune_core::metadata::matcher::batch_match_artist_mbids(db.clone()).await;
        tracing::info!(matched, "batch_artist_mbid_phase_complete");

        // Phase 2: Fetch images for all artists with MBID but no image
        tune_core::library::artwork::batch_enrich_artist_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artist enrichment started (Phase 1: MBID matching, Phase 2: image fetch)",
            "artists_without_mbid": without_mbid,
            "artists_without_image": missing.len(),
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artist_artwork_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("artist_artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let still_missing = artist_repo.list_without_image().unwrap_or_default().len();

    Json(json!({
        "result": result,
        "artists_without_image": still_missing,
    }))
}

pub(super) async fn rescan_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if tracks.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no tracks in album"})),
        )
            .into_response();
    }
    let cache_dir = artwork_cache_dir();
    let mut found_hash: Option<String> = None;
    for track in &tracks {
        if let Some(ref file_path) = track.file_path {
            if let Some(hash) = tune_core::library::artwork::get_or_extract(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                found_hash = Some(hash);
                break;
            }
        }
    }
    if let Some(ref hash) = found_hash {
        album_repo.force_update_cover_path(id, hash).ok();
    }
    Json(json!({
        "album_id": id,
        "rescanned_tracks": tracks.len(),
        "artwork_found": found_hash.is_some(),
        "hash": found_hash,
    }))
    .into_response()
}

pub(super) async fn rescan_all_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let backend = state.backend.clone();

    tokio::spawn(async move {
        let albums: Vec<i64> = backend
            .query_many("SELECT id FROM albums", &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.first().and_then(|v| v.as_i64()))
            .collect();

        let track_repo = TrackRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend);
        let mut updated = 0i32;
        for album_id in &albums {
            let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();
            for track in &tracks {
                if let Some(ref file_path) = track.file_path {
                    if let Some(hash) = tune_core::library::artwork::get_or_extract(
                        std::path::Path::new(file_path),
                        &cache_dir,
                    ) {
                        album_repo.force_update_cover_path(*album_id, &hash).ok();
                        updated += 1;
                        break;
                    }
                }
            }
        }
        tracing::info!(updated, total = albums.len(), "rescan_all_artwork done");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "message": "artwork rescan started"})),
    )
}
