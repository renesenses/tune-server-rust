use std::collections::HashSet;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;

use crate::state::AppState;

#[derive(Deserialize)]
struct Pagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct CreatePlaylist {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct UpdatePlaylist {
    name: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct AddTracks {
    track_ids: Vec<i64>,
    position: Option<i64>,
}

#[derive(Deserialize)]
struct RemoveTrack {
    position: i64,
}

#[derive(Deserialize)]
struct RemoveTracksBody {
    positions: Vec<i64>,
}

#[derive(Deserialize)]
struct ReorderTracksBody {
    track_ids: Vec<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_playlists).post(create_playlist))
        .route("/all", get(list_all_playlists))
        .route("/shared/{token}", get(get_shared_playlist))
        .route("/transfer", post(transfer_playlist))
        .route("/diff", post(diff_playlists))
        .route("/import/m3u", post(import_m3u_file))
        .route("/import/m3u-url", post(import_m3u_url))
        .route(
            "/{id}",
            get(get_playlist)
                .put(update_playlist)
                .delete(delete_playlist),
        )
        .route(
            "/{id}/tracks",
            get(get_tracks)
                .post(add_tracks)
                .delete(remove_tracks_batch)
                .put(reorder_tracks),
        )
        .route("/{id}/tracks/remove", post(remove_track))
        .route("/{id}/duplicate", post(duplicate_playlist))
        .route("/{id}/export", get(export_m3u))
        .route("/{id}/share", post(share_playlist))
        .route("/{id}/recover", post(recover_playlist))
        .route("/{id}/recover/apply", post(apply_recovery))
        .route(
            "/collaborative",
            get(list_collaborative).post(create_collaborative),
        )
        .route("/collaborative/{id}", get(get_collaborative))
        .route("/collaborative/{id}/add", post(add_to_collaborative))
        .route("/collaborative/{id}/tracks", get(collaborative_tracks))
        .route("/match", post(match_tracks))
}

async fn list_playlists(State(state): State<AppState>, Query(p): Query<Pagination>) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    let _total = repo.count().unwrap_or(0);
    Json(json!(items))
}

async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(pl)) => Json(json!(pl)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreatePlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.create(&body.name, body.description.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdatePlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.update(id, body.name.as_deref(), body.description.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tracks(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db.clone());
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::new(state.db)
        .get_multiple(&track_ids)
        .unwrap_or_default();
    Json(json!(tracks))
}

async fn add_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddTracks>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.add_tracks(id, &body.track_ids, body.position) {
        Ok(ids) => (StatusCode::CREATED, Json(json!({ "added": ids.len() }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveTrack>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.remove_track(id, body.position) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_tracks_batch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveTracksBody>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.remove_tracks_at_positions(id, &body.positions) {
        Ok(removed) => Json(json!({"removed": removed})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn reorder_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ReorderTracksBody>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.reorder_tracks(id, &body.track_ids) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn duplicate_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db.clone());
    let original = match repo.get(id) {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let new_name = format!("{} (copy)", original.name);
    let new_id = match repo.create(&new_name, None) {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    if !track_ids.is_empty() {
        repo.add_tracks(new_id, &track_ids, None).ok();
    }

    (
        StatusCode::CREATED,
        Json(json!({ "id": new_id, "name": new_name })),
    )
        .into_response()
}

async fn export_m3u(State(state): State<AppState>, Path(id): Path<i64>) -> Result<impl IntoResponse, AppError> {
    let repo = PlaylistRepo::new(state.db.clone());
    let playlist = match repo.get(id) {
        Ok(Some(p)) => p,
        _ => return Err(AppError::not_found("playlist not found")),
    };

    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::new(state.db)
        .get_multiple(&track_ids)
        .unwrap_or_default();

    let mut m3u = String::from("#EXTM3U\n");
    for t in &tracks {
        let duration_secs = t.duration_ms / 1000;
        let artist = t.artist_name.as_deref().unwrap_or("Unknown");
        m3u.push_str(&format!(
            "#EXTINF:{},{} - {}\n",
            duration_secs, artist, t.title
        ));
        if let Some(ref path) = t.file_path {
            m3u.push_str(path);
            m3u.push('\n');
        }
    }

    let filename = format!("{}.m3u", playlist.name.replace(' ', "_"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_static("audio/x-mpegurl; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|e| AppError::internal(e.to_string()))?,
    );

    Ok((axum::http::StatusCode::OK, headers, m3u))
}

async fn import_m3u_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut file_content = String::new();
    let mut playlist_name: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                if playlist_name.is_none() {
                    playlist_name = field.file_name().map(|f| {
                        f.trim_end_matches(".m3u8")
                            .trim_end_matches(".m3u")
                            .to_string()
                    });
                }
                file_content = field.text().await.unwrap_or_default();
            }
            "name" => {
                playlist_name = Some(field.text().await.unwrap_or_default());
            }
            _ => {}
        }
    }

    if file_content.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no file provided"})),
        )
            .into_response();
    }

    let name = playlist_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Imported Playlist".into());

    // Parse M3U and match tracks
    let mut track_ids: Vec<i64> = Vec::new();
    let mut total_entries = 0u32;
    let mut matched = 0u32;
    let mut not_found_paths: Vec<String> = Vec::new();

    let track_repo = TrackRepo::new(state.db.clone());

    for line in file_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        total_entries += 1;

        // Try exact path match first
        if let Ok(Some(track)) = track_repo.get_by_path(line) {
            if let Some(id) = track.id {
                track_ids.push(id);
                matched += 1;
                continue;
            }
        }

        // Try matching by filename (stem) via search
        let filename = std::path::Path::new(line)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(line);
        if let Ok(results) = track_repo.search(filename, 1) {
            if let Some(track) = results.first() {
                if let Some(id) = track.id {
                    track_ids.push(id);
                    matched += 1;
                    continue;
                }
            }
        }

        not_found_paths.push(line.to_string());
    }

    // Create playlist and add tracks
    let repo = PlaylistRepo::new(state.db);
    match repo.create(&name, None) {
        Ok(playlist_id) => {
            if !track_ids.is_empty() {
                repo.add_tracks(playlist_id, &track_ids, None).ok();
            }
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": playlist_id,
                    "name": name,
                    "total_entries": total_entries,
                    "matched": matched,
                    "not_found": not_found_paths.len(),
                    "not_found_paths": not_found_paths,
                    "track_count": track_ids.len(),
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct TransferPlaylist {
    playlist_id: i64,
    #[allow(dead_code)]
    target_service: Option<String>,
    zone_id: Option<i64>,
}

#[derive(Deserialize)]
struct DiffPlaylists {
    playlist_id_a: i64,
    playlist_id_b: i64,
}

#[derive(Deserialize)]
struct ImportM3uUrl {
    url: String,
    name: Option<String>,
}

async fn import_m3u_url(
    State(state): State<AppState>,
    Json(body): Json<ImportM3uUrl>,
) -> impl IntoResponse {
    let m3u_content = match reqwest::get(&body.url).await {
        Ok(resp) => match resp.text().await {
            Ok(text) => text,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("read failed: {e}")).into_response();
            }
        },
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("fetch failed: {e}")).into_response(),
    };

    let name = body.name.unwrap_or_else(|| "Imported Playlist".into());
    let repo = PlaylistRepo::new(state.db.clone());
    let playlist_id = match repo.create(&name, None) {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let track_repo = TrackRepo::new(state.db);
    let mut matched = 0i64;

    for line in m3u_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(Some(track)) = track_repo.get_by_path(line)
            && let Some(id) = track.id
        {
            repo.add_tracks(playlist_id, &[id], None).ok();
            matched += 1;
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "id": playlist_id,
            "name": name,
            "matched_tracks": matched,
        })),
    )
        .into_response()
}

// --- Advanced playlist routes ---

async fn list_all_playlists(State(state): State<AppState>) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db);
    let items = repo.list(99999, 0).unwrap_or_default();
    Json(json!(items))
}

async fn share_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db.clone());
    match repo.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }

    // Generate a pseudo-random token from high-resolution clock
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let token = format!("{:032x}", nanos ^ (id as u128 * 0x517cc1b727220a95));
    let settings = SettingsRepo::new(state.db);
    let key = format!("playlist_share_{id}");
    if let Err(e) = settings.set(&key, &token) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({
        "token": token,
        "url": format!("/api/v1/playlists/shared/{token}"),
    }))
    .into_response()
}

async fn get_shared_playlist(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let all = settings.all().unwrap_or_default();

    let playlist_id = all
        .iter()
        .find(|(k, v)| k.starts_with("playlist_share_") && v == &token)
        .and_then(|(k, _)| {
            k.strip_prefix("playlist_share_")
                .and_then(|s| s.parse::<i64>().ok())
        });

    let playlist_id = match playlist_id {
        Some(id) => id,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let repo = PlaylistRepo::new(state.db.clone());
    let playlist = match repo.get(playlist_id) {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let track_ids = repo.get_track_ids(playlist_id).unwrap_or_default();
    let tracks = TrackRepo::new(state.db)
        .get_multiple(&track_ids)
        .unwrap_or_default();

    Json(json!({
        "playlist": playlist,
        "tracks": tracks,
    }))
    .into_response()
}

async fn recover_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(pl)) => Json(json!(pl)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn transfer_playlist(
    State(state): State<AppState>,
    Json(body): Json<TransferPlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db.clone());
    let track_ids = match repo.get_track_ids(body.playlist_id) {
        Ok(ids) => ids,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let zone_id = body.zone_id.unwrap_or(1);
    let queue = PlayQueueRepo::new(state.db);
    if let Err(e) = queue.add_tracks(zone_id, &track_ids, None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({ "transferred": track_ids.len() })).into_response()
}

async fn diff_playlists(
    State(state): State<AppState>,
    Json(body): Json<DiffPlaylists>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    let ids_a: HashSet<i64> = repo
        .get_track_ids(body.playlist_id_a)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let ids_b: HashSet<i64> = repo
        .get_track_ids(body.playlist_id_b)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let only_a: Vec<i64> = ids_a.difference(&ids_b).copied().collect();
    let only_b: Vec<i64> = ids_b.difference(&ids_a).copied().collect();
    let common: Vec<i64> = ids_a.intersection(&ids_b).copied().collect();

    Json(json!({
        "only_in_a": only_a,
        "only_in_b": only_b,
        "common": common,
        "count_only_a": only_a.len(),
        "count_only_b": only_b.len(),
        "count_common": common.len(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Recovery apply
// ---------------------------------------------------------------------------

async fn apply_recovery(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db.clone());
    let track_repo = TrackRepo::new(state.db);
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let mut recovered = 0i64;
    let mut missing = 0i64;

    for tid in &track_ids {
        match track_repo.get(*tid) {
            Ok(Some(t)) if t.file_path.is_some() => {
                let path = t.file_path.as_ref().unwrap();
                if std::path::Path::new(path).exists() {
                    recovered += 1;
                } else {
                    missing += 1;
                }
            }
            _ => missing += 1,
        }
    }

    Json(json!({
        "playlist_id": id,
        "total_tracks": track_ids.len(),
        "recovered": recovered,
        "still_missing": missing,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Collaborative playlists (stored in settings as JSON)
// ---------------------------------------------------------------------------

async fn list_collaborative(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(items))
}

#[derive(Deserialize)]
struct CreateCollaborative {
    name: String,
    description: Option<String>,
}

async fn create_collaborative(
    State(state): State<AppState>,
    Json(body): Json<CreateCollaborative>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let id = format!("collab_{:016x}", nanos);

    let entry = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "tracks": [],
        "created_at": nanos / 1_000_000_000,
    });
    items.push(entry.clone());
    settings
        .set(
            "collaborative_playlists",
            &serde_json::to_string(&items).unwrap_or_default(),
        )
        .ok();
    (StatusCode::CREATED, Json(entry)).into_response()
}

async fn get_collaborative(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    match items.iter().find(|i| i["id"].as_str() == Some(&id)) {
        Some(item) => Json(item.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct AddToCollaborative {
    track_ids: Vec<i64>,
}

async fn add_to_collaborative(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddToCollaborative>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let Some(entry) = items.iter_mut().find(|i| i["id"].as_str() == Some(&id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let tracks = entry.get_mut("tracks").and_then(|t| t.as_array_mut());
    if let Some(tracks) = tracks {
        for tid in &body.track_ids {
            tracks.push(json!(tid));
        }
    }
    settings
        .set(
            "collaborative_playlists",
            &serde_json::to_string(&items).unwrap_or_default(),
        )
        .ok();
    Json(json!({ "added": body.track_ids.len() })).into_response()
}

async fn collaborative_tracks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let Some(entry) = items.iter().find(|i| i["id"].as_str() == Some(&id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let track_ids: Vec<i64> = entry["tracks"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    let track_repo = TrackRepo::new(state.db);
    let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
    Json(json!(tracks)).into_response()
}

// ---------------------------------------------------------------------------
// Match tracks (fuzzy search)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MatchEntry {
    title: String,
    artist: Option<String>,
}

#[derive(Deserialize)]
struct MatchRequest {
    tracks: Vec<MatchEntry>,
}

async fn match_tracks(
    State(state): State<AppState>,
    Json(body): Json<MatchRequest>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::new(state.db);
    let mut results: Vec<Value> = Vec::new();

    for entry in &body.tracks {
        let q = if let Some(ref artist) = entry.artist {
            format!("{} {}", artist, entry.title)
        } else {
            entry.title.clone()
        };
        let matched = track_repo.search(&q, 3).unwrap_or_default();
        results.push(json!({
            "query_title": entry.title,
            "query_artist": entry.artist,
            "matches": matched,
        }));
    }

    Json(json!({ "results": results, "total": body.tracks.len() })).into_response()
}
