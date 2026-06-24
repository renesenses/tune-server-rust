use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct BackupRequest {
    pub playlist_id: i64,
}

/// POST /system/playlist-hub/backup — backup a local playlist to cloud
pub(super) async fn backup(
    State(state): State<AppState>,
    Json(body): Json<BackupRequest>,
) -> impl IntoResponse {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    if instance_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "instance_id not configured"})),
        )
            .into_response();
    }

    match tune_core::cloud::playlist_hub::backup_playlist(
        &state.backend,
        &state.http_client,
        &instance_id,
        body.playlist_id,
    )
    .await
    {
        Ok(hub_id) => Json(json!({
            "hub_id": hub_id,
            "playlist_id": body.playlist_id,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// GET /system/playlist-hub — list cloud playlists for this instance
pub(super) async fn list_playlists(State(state): State<AppState>) -> impl IntoResponse {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    if instance_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "instance_id not configured"})),
        )
            .into_response();
    }

    match tune_core::cloud::playlist_hub::list_cloud_playlists(&state.http_client, &instance_id)
        .await
    {
        Ok(playlists) => Json(json!({"playlists": playlists})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"playlists": [], "error": e})),
        )
            .into_response(),
    }
}

/// GET /system/playlist-hub/{hub_id} — get cloud playlist detail with tracks
pub(super) async fn get_playlist(
    State(state): State<AppState>,
    Path(hub_id): Path<String>,
) -> impl IntoResponse {
    match tune_core::cloud::playlist_hub::get_cloud_playlist(&state.http_client, &hub_id).await {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// DELETE /system/playlist-hub/{hub_id} — delete a cloud playlist
pub(super) async fn delete_playlist(
    State(state): State<AppState>,
    Path(hub_id): Path<String>,
) -> impl IntoResponse {
    match tune_core::cloud::playlist_hub::delete_cloud_playlist(&state.http_client, &hub_id).await {
        Ok(()) => Json(json!({"deleted": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct TransferRequest {
    pub target_service: String,
}

/// POST /system/playlist-hub/{hub_id}/transfer — initiate transfer to another service
pub(super) async fn transfer(
    State(state): State<AppState>,
    Path(hub_id): Path<String>,
    Json(body): Json<TransferRequest>,
) -> impl IntoResponse {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    if instance_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "instance_id not configured"})),
        )
            .into_response();
    }

    let valid_services = ["qobuz", "tidal", "spotify", "deezer", "youtube"];
    if !valid_services.contains(&body.target_service.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid target_service: {}", body.target_service)})),
        )
            .into_response();
    }

    match tune_core::cloud::playlist_hub::request_transfer(
        &state.http_client,
        &instance_id,
        &hub_id,
        &body.target_service,
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}
