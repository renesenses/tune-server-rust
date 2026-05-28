use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tags).post(create_tag))
        .route("/{id}", get(get_tag).put(update_tag).delete(delete_tag))
        .route("/{id}/items", get(list_tag_items).post(add_tag_item))
        .route(
            "/{id}/items/{item_type}/{item_id}",
            axum::routing::delete(remove_tag_item),
        )
}

async fn list_tags(State(state): State<AppState>) -> Json<Value> {
    let repo = TagRepo::new(state.db);
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn create_tag(
    State(state): State<AppState>,
    Json(body): Json<CreateTag>,
) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.create(&body.name, body.color.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
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
    let repo = TagRepo::new(state.db);
    match repo.update(id, body.name.as_deref(), body.color.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn list_tag_items(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let repo = TagRepo::new(state.db);
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
    let repo = TagRepo::new(state.db);
    match repo.tag_item(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_tag_item(
    State(state): State<AppState>,
    Path((id, item_type, item_id)): Path<(i64, String, i64)>,
) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.untag_item(id, &item_type, item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
