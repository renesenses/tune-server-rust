use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

use super::now_iso_utc;

#[derive(Deserialize)]
pub(super) struct CreateCollectionBody {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CollectionAlbumPath {
    id: i64,
    album_id: i64,
}

pub(super) async fn list_collections(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let data = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();
    Json(json!(data))
}

pub(super) async fn create_collection(
    State(state): State<AppState>,
    Json(body): Json<CreateCollectionBody>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = collections
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(0)
        + 1;

    let collection = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "album_ids": [],
        "created_at": now_iso_utc(),
    });
    collections.push(collection.clone());
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();

    Ok((StatusCode::CREATED, Json(collection)))
}

pub(super) async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    match found {
        Some(c) => Json(c.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = collections.len();
    collections.retain(|c| c.get("id").and_then(|v| v.as_i64()) != Some(id));
    if collections.len() == before {
        return Err(AppError::not_found("collection not found"));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let album_ids: Vec<i64> = collection
        .get("album_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let albums: Vec<Value> = album_ids
        .iter()
        .filter_map(|&aid| album_repo.get(aid).ok().flatten().map(|a| a.to_json()))
        .collect();
    Json(json!(albums)).into_response()
}

pub(super) async fn add_album_to_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    let album_ids = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut());
    match album_ids {
        Some(arr) => {
            let already = arr.iter().any(|v| v.as_i64() == Some(path.album_id));
            if !already {
                arr.push(json!(path.album_id));
            }
        }
        None => {
            if let Some(obj) = collection.as_object_mut() {
                obj.insert("album_ids".into(), json!([path.album_id]));
            }
        }
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"added": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}

pub(super) async fn remove_album_from_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    if let Some(arr) = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| v.as_i64() != Some(path.album_id));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"removed": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}
