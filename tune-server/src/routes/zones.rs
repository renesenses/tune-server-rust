use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateZone {
    name: String,
    output_type: Option<String>,
    output_device_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateVolume {
    volume: i32,
}

#[derive(Deserialize)]
struct UpdateMuted {
    muted: bool,
}

#[derive(Deserialize)]
struct RenameZone {
    name: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_zones).post(create_zone))
        .route("/{id}", get(get_zone).delete(delete_zone))
        .route("/{id}/volume", put(update_volume))
        .route("/{id}/muted", put(update_muted))
        .route("/{id}/name", put(rename_zone))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/list", get(list_groups))
        .route("/groups/{group_id}", axum::routing::delete(delete_group))
        .route("/stereo-pairs", get(list_stereo_pairs).post(create_stereo_pair))
        .route("/stereo-pairs/{pair_id}", axum::routing::delete(delete_stereo_pair))
}

async fn list_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = ZoneRepo::new(state.db.clone());
    let zones = repo.list().unwrap_or_default();
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let mut v = serde_json::to_value(z).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("state".into(), json!(match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            }));
            obj.insert("current_track".into(), json!(ps.now_playing));
            obj.insert("position_ms".into(), json!(ps.position_ms));
            obj.insert("queue_length".into(), json!(ps.queue_length));
            obj.insert("volume".into(), json!(z.volume as f64 / 100.0));
        }
        result.push(v);
    }
    Json(json!(result))
}

async fn get_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(zone)) => Json(json!(zone)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_zone(
    State(state): State<AppState>,
    Json(body): Json<CreateZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.create(&body.name, body.output_type.as_deref(), body.output_device_id.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateVolume>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_volume(id, body.volume) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_muted(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMuted>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_muted(id, body.muted) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn rename_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::new(state.db);
    match repo.update_name(id, &body.name) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateGroup {
    name: String,
    zone_ids: Vec<i64>,
}

async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(groups))
}

async fn create_group(
    State(state): State<AppState>,
    Json(body): Json<CreateGroup>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = groups.len() as i64 + 1;
    groups.push(json!({
        "id": id,
        "name": body.name,
        "zone_ids": body.zone_ids,
    }));

    settings.set("zone_groups", &serde_json::to_string(&groups).unwrap()).ok();
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    groups.retain(|g| g.get("id").and_then(|v| v.as_i64()) != Some(group_id));
    settings.set("zone_groups", &serde_json::to_string(&groups).unwrap()).ok();
    StatusCode::NO_CONTENT
}

async fn list_stereo_pairs(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(pairs))
}

#[derive(Deserialize)]
struct CreateStereoPair {
    name: String,
    left_device_id: String,
    right_device_id: String,
}

async fn create_stereo_pair(
    State(state): State<AppState>,
    Json(body): Json<CreateStereoPair>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = pairs.len() as i64 + 1;
    pairs.push(json!({
        "id": id,
        "name": body.name,
        "left_device_id": body.left_device_id,
        "right_device_id": body.right_device_id,
    }));

    settings.set("stereo_pairs", &serde_json::to_string(&pairs).unwrap()).ok();
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

async fn delete_stereo_pair(
    State(state): State<AppState>,
    Path(pair_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    pairs.retain(|p| p.get("id").and_then(|v| v.as_i64()) != Some(pair_id));
    settings.set("stereo_pairs", &serde_json::to_string(&pairs).unwrap()).ok();
    StatusCode::NO_CONTENT
}
