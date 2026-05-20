use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;

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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_playlists).post(create_playlist))
        .route("/{id}", get(get_playlist).put(update_playlist).delete(delete_playlist))
        .route("/{id}/tracks", get(get_tracks).post(add_tracks))
        .route("/{id}/tracks/remove", post(remove_track))
        .route("/{id}/duplicate", post(duplicate_playlist))
        .route("/{id}/export", get(export_m3u))
        .route("/import/m3u-url", post(import_m3u_url))
}

async fn list_playlists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    let _total = repo.count().unwrap_or(0);
    Json(json!(items))
}

async fn get_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
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

async fn delete_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db.clone());
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::new(state.db).get_multiple(&track_ids).unwrap_or_default();
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

    (StatusCode::CREATED, Json(json!({ "id": new_id, "name": new_name }))).into_response()
}

async fn export_m3u(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::new(state.db.clone());
    let playlist = match repo.get(id) {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let tracks = TrackRepo::new(state.db).get_multiple(&track_ids).unwrap_or_default();

    let mut m3u = String::from("#EXTM3U\n");
    for t in &tracks {
        let duration_secs = t.duration_ms / 1000;
        let artist = t.artist_name.as_deref().unwrap_or("Unknown");
        m3u.push_str(&format!("#EXTINF:{},{} - {}\n", duration_secs, artist, t.title));
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
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")).unwrap(),
    );

    (axum::http::StatusCode::OK, headers, m3u).into_response()
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
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("read failed: {e}")).into_response(),
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
            && let Some(id) = track.id {
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
