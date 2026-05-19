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
}

async fn list_playlists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = PlaylistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    let total = repo.count().unwrap_or(0);
    Json(json!({ "items": items, "total": total, "limit": limit, "offset": offset }))
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
    Json(json!({ "items": tracks, "total": tracks.len() }))
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
