use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::tag_repo::TagRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateTag {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
struct UpdateTag {
    name: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
struct AddTagItem {
    item_type: String,
    item_id: i64,
}

#[derive(Deserialize)]
struct BatchTagRequest {
    item_type: String,
    item_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct TagSearchQuery {
    q: Option<String>,
    item_type: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tags).post(create_tag))
        .route("/search", get(search_tags))
        .route("/{id}", get(get_tag).put(update_tag).delete(delete_tag))
        .route("/{id}/items", get(list_tag_items).post(add_tag_item))
        .route("/{id}/items/batch", post(batch_tag_items))
        .route("/{id}/items/batch-remove", post(batch_untag_items))
        .route(
            "/{id}/items/{item_type}/{item_id}",
            axum::routing::delete(remove_tag_item),
        )
        .route("/{id}/albums", get(list_tag_albums))
        .route("/{id}/tracks", get(list_tag_tracks))
        .route("/{id}/artists", get(list_tag_artists))
        .route("/for/{item_type}/{item_id}", get(tags_for_item))
}

async fn list_tags(State(state): State<AppState>, Query(q): Query<TagSearchQuery>) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let items = repo
        .list_with_counts(q.item_type.as_deref())
        .unwrap_or_default();
    Json(json!(items))
}

async fn search_tags(
    State(state): State<AppState>,
    Query(q): Query<TagSearchQuery>,
) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let query = q.q.unwrap_or_default();
    if query.is_empty() {
        let tags = repo.list().unwrap_or_default();
        return Json(json!(tags));
    }
    let tags = repo.search(&query).unwrap_or_default();
    Json(json!(tags))
}

async fn create_tag(
    State(state): State<AppState>,
    Json(body): Json<CreateTag>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    if let Ok(Some(existing)) = repo.get_by_name(&body.name) {
        return Json(json!({ "id": existing.id, "exists": true })).into_response();
    }
    match repo.create(&body.name, body.color.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(tag)) => Json(json!(tag)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateTag>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.color.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn list_tag_items(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let items = repo.all_items_by_tag(id).unwrap_or_default();
    let items: Vec<Value> = items
        .into_iter()
        .map(|(item_type, item_id)| json!({"item_type": item_type, "item_id": item_id}))
        .collect();
    Json(json!({"tag_id": id, "items": items}))
}

async fn add_tag_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddTagItem>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.tag_item(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn batch_tag_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<BatchTagRequest>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.batch_tag(id, &body.item_type, &body.item_ids) {
        Ok(count) => Json(json!({"tagged": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn batch_untag_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<BatchTagRequest>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.batch_untag(id, &body.item_type, &body.item_ids) {
        Ok(count) => Json(json!({"untagged": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_tag_item(
    State(state): State<AppState>,
    Path((id, item_type, item_id)): Path<(i64, String, i64)>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.untag_item(id, &item_type, item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn list_tag_albums(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let album_ids = tag_repo.items_by_tag(id, "album").unwrap_or_default();
    let album_repo = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone());
    let albums: Vec<Value> = album_ids
        .into_iter()
        .filter_map(|aid| album_repo.get(aid).ok().flatten())
        .map(|a| a.to_json())
        .collect();
    Json(json!({"tag_id": id, "albums": albums, "count": albums.len()}))
}

async fn list_tag_tracks(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let track_ids = tag_repo.items_by_tag(id, "track").unwrap_or_default();
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let tracks: Vec<Value> = track_ids
        .into_iter()
        .filter_map(|tid| track_repo.get(tid).ok().flatten())
        .map(|t| t.to_json())
        .collect();
    Json(json!({"tag_id": id, "tracks": tracks, "count": tracks.len()}))
}

async fn list_tag_artists(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let artist_ids = tag_repo.items_by_tag(id, "artist").unwrap_or_default();
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let artists: Vec<Value> = artist_ids
        .into_iter()
        .filter_map(|aid| artist_repo.get(aid).ok().flatten())
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "image_path": a.image_path,
            })
        })
        .collect();
    Json(json!({"tag_id": id, "artists": artists, "count": artists.len()}))
}

async fn tags_for_item(
    State(state): State<AppState>,
    Path((item_type, item_id)): Path<(String, i64)>,
) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let tags = repo.tags_for_item(&item_type, item_id).unwrap_or_default();
    Json(json!(tags))
}
