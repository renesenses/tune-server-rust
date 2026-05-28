use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms", get(list_rooms))
        .route("/rooms/{id}/play", post(play_room))
        .route("/rooms/{id}/pause", post(pause_room))
        .route("/rooms/{id}/volume", post(set_room_volume))
        .route("/rooms/{id}/group", post(group_rooms))
}

/// Return DLNA devices whose manufacturer or model contains "Sonos".
async fn list_rooms(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;
    drop(scanner);

    let sonos: Vec<Value> = devices
        .iter()
        .filter(|d| {
            let mfr = d.manufacturer.as_deref().unwrap_or("").to_lowercase();
            let model = d.model.as_deref().unwrap_or("").to_lowercase();
            mfr.contains("sonos") || model.contains("sonos")
        })
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "host": d.host,
                "port": d.port,
                "manufacturer": d.manufacturer,
                "model": d.model,
                "available": d.available,
            })
        })
        .collect();

    Json(json!(sonos))
}

/// Send a Play (resume) command to the given Sonos device via its DLNA output.
async fn play_room(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    match output.resume().await {
        Ok(()) => Json(json!({"status": "playing"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Send a Pause command to the given Sonos device via its DLNA output.
async fn pause_room(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    match output.pause().await {
        Ok(()) => Json(json!({"status": "paused"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: f64,
}

/// Set volume on the given Sonos device via its DLNA output.
/// Volume is a float 0.0..1.0 matching the OutputTarget trait.
async fn set_room_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    match output.set_volume(body.volume).await {
        Ok(()) => Json(json!({"volume": body.volume})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct GroupBody {
    room_ids: Vec<String>,
}

/// Group rooms together. This is a placeholder -- real Sonos grouping
/// requires the Sonos-specific household/group API or UPnP group rendering.
async fn group_rooms(Path(id): Path<String>, Json(body): Json<GroupBody>) -> Json<Value> {
    Json(json!({
        "coordinator": id,
        "members": body.room_ids,
        "status": "grouping not yet implemented — requires Sonos household API",
    }))
}
