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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    let _total = repo.count().unwrap_or(0);
    Json(json!(items))
}

async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.description.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tracks(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .unwrap_or_default();
    Json(json!(tracks))
}

async fn add_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddTracks>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.reorder_tracks(id, &body.track_ids) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn duplicate_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export_m3u(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<impl IntoResponse, AppError> {
    let fmt = q.format.as_deref().unwrap_or("m3u");
    if fmt != "m3u" {
        return export_multi_format(State(state.clone()), id, fmt).await;
    }
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = match repo.get(id) {
        Ok(Some(p)) => p,
        _ => return Err(AppError::not_found("playlist not found")),
    };

    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::with_backend(state.backend.clone())
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

async fn export_multi_format(
    State(state): State<AppState>,
    id: i64,
    format: &str,
) -> Result<(axum::http::StatusCode, axum::http::HeaderMap, String), AppError> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("playlist not found"))?;
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .unwrap_or_default();

    let (content, content_type, ext) = match format {
        "json" => {
            let items: Vec<serde_json::Value> = tracks
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "title": t.title, "artist": t.artist_name, "album": t.album_title,
                        "duration_ms": t.duration_ms, "file_path": t.file_path,
                    })
                })
                .collect();
            (
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": playlist.name, "tracks": items}),
                )
                .unwrap_or_default(),
                "application/json",
                "json",
            )
        }
        "csv" => {
            let mut csv = String::from("title,artist,album,duration_ms,file_path\n");
            for t in &tracks {
                csv.push_str(&format!(
                    "\"{}\",\"{}\",\"{}\",{},\"{}\"\n",
                    t.title.replace('"', "\"\""),
                    t.artist_name.as_deref().unwrap_or("").replace('"', "\"\""),
                    t.album_title.as_deref().unwrap_or("").replace('"', "\"\""),
                    t.duration_ms,
                    t.file_path.as_deref().unwrap_or("").replace('"', "\"\""),
                ));
            }
            (csv, "text/csv", "csv")
        }
        "xspf" => {
            let mut xspf = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n",
            );
            xspf.push_str(&format!(
                "  <title>{}</title>\n  <trackList>\n",
                quick_xml::escape::escape(&playlist.name)
            ));
            for t in &tracks {
                xspf.push_str("    <track>\n");
                xspf.push_str(&format!(
                    "      <title>{}</title>\n",
                    quick_xml::escape::escape(&t.title)
                ));
                if let Some(ref a) = t.artist_name {
                    xspf.push_str(&format!(
                        "      <creator>{}</creator>\n",
                        quick_xml::escape::escape(a)
                    ));
                }
                if let Some(ref a) = t.album_title {
                    xspf.push_str(&format!(
                        "      <album>{}</album>\n",
                        quick_xml::escape::escape(a)
                    ));
                }
                xspf.push_str(&format!("      <duration>{}</duration>\n", t.duration_ms));
                if let Some(ref p) = t.file_path {
                    xspf.push_str(&format!(
                        "      <location>{}</location>\n",
                        quick_xml::escape::escape(p)
                    ));
                }
                xspf.push_str("    </track>\n");
            }
            xspf.push_str("  </trackList>\n</playlist>\n");
            (xspf, "application/xspf+xml", "xspf")
        }
        _ => {
            return Err(AppError::bad_request(
                "format must be m3u, json, csv, or xspf",
            ));
        }
    };

    let filename = format!("{}.{ext}", playlist.name.replace(' ', "_"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_str(content_type).unwrap(),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")).unwrap(),
    );
    Ok((axum::http::StatusCode::OK, headers, content))
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

    let track_repo = TrackRepo::with_backend(state.backend.clone());

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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    source_service: String,
    source_playlist_id: String,
    target_service: String,
    target_playlist_id: String,
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist_id = match repo.create(&name, None) {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let track_repo = TrackRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let items = repo.list(99999, 0).unwrap_or_default();
    Json(json!(items))
}

async fn share_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = match repo.get(playlist_id) {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let track_ids = repo.get_track_ids(playlist_id).unwrap_or_default();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .unwrap_or_default();

    Json(json!({
        "playlist": playlist,
        "tracks": tracks,
    }))
    .into_response()
}

async fn recover_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
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
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_ids = match repo.get_track_ids(body.playlist_id) {
        Ok(ids) => ids,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let zone_id = body.zone_id.unwrap_or(1);
    let queue = PlayQueueRepo::with_backend(state.backend.clone());
    if let Err(e) = queue.add_tracks(zone_id, &track_ids, None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({ "transferred": track_ids.len() })).into_response()
}

/// Fetch `(title, artist)` pairs for a playlist on a given service. Local reads
/// the DB; streaming services go through the registry. Used by the cross-service
/// diff, which matches on title+artist since the two sides share no track ids.
async fn diff_playlist_tracks(
    state: &AppState,
    service: &str,
    playlist_id: &str,
) -> Vec<(String, String)> {
    if service == "local" || service.is_empty() {
        let pid: i64 = playlist_id.parse().unwrap_or(0);
        let prepo = PlaylistRepo::with_backend(state.backend.clone());
        let trepo = TrackRepo::with_backend(state.backend.clone());
        prepo
            .get_track_ids(pid)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| trepo.get(id).ok().flatten())
            .map(|t| (t.title, t.artist_name.unwrap_or_default()))
            .collect()
    } else {
        let reg = state.services.lock().await;
        reg.get_playlist_tracks(service, playlist_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|v| {
                (
                    v.get("title")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    v.get("artist")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    }
}

async fn diff_playlists(
    State(state): State<AppState>,
    Json(body): Json<DiffPlaylists>,
) -> impl IntoResponse {
    let src = diff_playlist_tracks(&state, &body.source_service, &body.source_playlist_id).await;
    let tgt = diff_playlist_tracks(&state, &body.target_service, &body.target_playlist_id).await;

    let norm =
        |t: &str, a: &str| format!("{}|{}", t.trim().to_lowercase(), a.trim().to_lowercase());
    let src_keys: HashSet<String> = src.iter().map(|(t, a)| norm(t, a)).collect();
    let tgt_keys: HashSet<String> = tgt.iter().map(|(t, a)| norm(t, a)).collect();

    let entry = |t: &str, a: &str, in_s: bool, in_t: bool| {
        json!({
            "title": t, "artist_name": a,
            "in_source": in_s, "in_target": in_t,
            "match_quality": "exact",
        })
    };
    let only_in_source: Vec<Value> = src
        .iter()
        .filter(|(t, a)| !tgt_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, true, false))
        .collect();
    let in_both: Vec<Value> = src
        .iter()
        .filter(|(t, a)| tgt_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, true, true))
        .collect();
    let only_in_target: Vec<Value> = tgt
        .iter()
        .filter(|(t, a)| !src_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, false, true))
        .collect();

    // Best-effort display names: local playlists resolve to their name.
    let name_of = |service: &str, id: &str| -> String {
        if service == "local" || service.is_empty() {
            let prepo = PlaylistRepo::with_backend(state.backend.clone());
            id.parse::<i64>()
                .ok()
                .and_then(|pid| prepo.get(pid).ok().flatten())
                .map(|p| p.name)
                .unwrap_or_else(|| id.to_string())
        } else {
            service.to_string()
        }
    };

    Json(json!({
        "source_name": name_of(&body.source_service, &body.source_playlist_id),
        "target_name": name_of(&body.target_service, &body.target_playlist_id),
        "only_in_source": only_in_source,
        "only_in_target": only_in_target,
        "in_both": in_both,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Recovery apply
// ---------------------------------------------------------------------------

async fn apply_recovery(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

    let track_repo = TrackRepo::with_backend(state.backend.clone());
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
    let track_repo = TrackRepo::with_backend(state.backend.clone());
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
