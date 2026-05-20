use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::tag_repo::TagRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateTag {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
struct TagAction {
    tag_id: i64,
    item_type: String,
    item_id: i64,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tags).post(create_tag))
        .route("/{id}", get(get_tag_items).delete(delete_tag))
        .route("/add", post(tag_item))
        .route("/remove", post(untag_item))
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

async fn delete_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tag_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TagRepo::new(state.db);
    let tracks = repo.items_by_tag(id, "track").unwrap_or_default();
    let albums = repo.items_by_tag(id, "album").unwrap_or_default();
    Json(json!({
        "tag_id": id,
        "tracks": tracks,
        "albums": albums,
    }))
}

async fn tag_item(
    State(state): State<AppState>,
    Json(body): Json<TagAction>,
) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.tag_item(body.tag_id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn untag_item(
    State(state): State<AppState>,
    Json(body): Json<TagAction>,
) -> impl IntoResponse {
    let repo = TagRepo::new(state.db);
    match repo.untag_item(body.tag_id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
