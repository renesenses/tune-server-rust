use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
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
    for ext in &["jpg", "png"] {
        let path = cache_dir.join(format!("{hash}.{ext}"));
        if path.exists()
            && let Ok(data) = tokio::fs::read(&path).await
        {
            let mime = if *ext == "png" {
                "image/png"
            } else {
                "image/jpeg"
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
    let repo = AlbumRepo::new(state.db.clone());
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

    let track_repo = TrackRepo::new(state.db);
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
    let repo = AlbumRepo::new(state.db.clone());
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

    // Skip if album already has a cover
    if album.cover_path.is_some() {
        return Json(json!({"enriched": false, "reason": "album already has cover art"}))
            .into_response();
    }

    let Some(ref mbid) = album.musicbrainz_release_id else {
        return Json(json!({"enriched": false, "reason": "no MusicBrainz release ID"}))
            .into_response();
    };

    match tune_core::library::artwork::fetch_cover_art(mbid).await {
        Some(data) => {
            let cache_dir = artwork_cache_dir();
            let hash = tune_core::library::artwork::artwork_hash(mbid);
            if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, "jpg").is_some()
            {
                repo.update_cover_path(id, &hash).ok();
                Json(json!({"enriched": true, "hash": hash, "size": data.len()})).into_response()
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
    let db = state.db.clone();

    // Check how many albums are missing covers
    let album_repo = AlbumRepo::new(state.db.clone());
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
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
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
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let result = settings
        .get("artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let album_repo = AlbumRepo::new(state.db);
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
    let db = state.db.clone();

    // Check how many artists are missing images
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(state.db.clone());
    let missing = artist_repo.list_without_image().unwrap_or_default();

    if missing.is_empty() {
        return Json(json!({
            "status": "skipped",
            "message": "all artists with MBID already have images",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    settings.set("artist_artwork_enrich_status", "running").ok();
    settings
        .set(
            "artist_artwork_enrich_result",
            &json!({"total": missing.len(), "enriched": 0, "status": "running"}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artist_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artist artwork enrichment started",
            "artists_to_process": missing.len(),
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artist_artwork_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let result = settings
        .get("artist_artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(state.db);
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
    let track_repo = TrackRepo::new(state.db.clone());
    let album_repo = AlbumRepo::new(state.db);
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
    let db = state.db.clone();

    tokio::spawn(async move {
        let albums: Vec<i64> = db
            .query_many("SELECT id FROM albums", &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.first().and_then(|v| v.as_i64()))
            .collect();

        let track_repo = TrackRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db);
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
