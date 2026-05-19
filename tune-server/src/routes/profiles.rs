use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::profile_repo::ProfileRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateProfile {
    username: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProfile {
    display_name: Option<String>,
    avatar_path: Option<String>,
}

#[derive(Deserialize)]
struct FavoriteAction {
    item_type: String,
    item_id: i64,
}

#[derive(Deserialize)]
struct FavoritesQuery {
    item_type: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_profiles).post(create_profile))
        .route("/{id}", get(get_profile).put(update_profile).delete(delete_profile))
        .route("/{id}/favorites", get(list_favorites))
        .route("/{id}/favorites/add", post(add_favorite))
        .route("/{id}/favorites/remove", post(remove_favorite))
}

async fn list_profiles(State(state): State<AppState>) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let items = repo.list().unwrap_or_default();
    Json(json!({ "items": items, "total": items.len() }))
}

async fn get_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(p)) => Json(json!(p)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.create(&body.username, body.display_name.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.update(id, body.display_name.as_deref(), body.avatar_path.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn list_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<FavoritesQuery>,
) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let items = repo
        .list_favorites(id, q.item_type.as_deref())
        .unwrap_or_default();
    Json(json!({ "items": items, "total": items.len() }))
}

async fn add_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.add_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.remove_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
