use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/enable/{zone_id}", post(enable_dj))
        .route("/disable/{zone_id}", post(disable_dj))
        .route("/status/{zone_id}", get(dj_status))
        .route("/play", post(dj_play))
        .route("/pause", post(dj_pause))
        .route("/crossfade", post(dj_crossfade))
        .route("/crossfader", post(dj_crossfader))
        .route("/auto-crossfade", post(dj_auto_crossfade))
        .route("/load/{zone_id}/{deck}", post(dj_load))
        .route("/volume/{zone_id}/{deck}", post(dj_volume))
        .route("/sync-tempo/{zone_id}", post(dj_sync_tempo))
        .route("/waveform/{track_id}", get(dj_waveform))
        .route("/analyze/{track_id}", post(dj_analyze))
}

async fn enable_dj(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set(&format!("dj_enabled_{zone_id}"), "true").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": true}))
}

async fn disable_dj(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set(&format!("dj_enabled_{zone_id}"), "false").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": false}))
}

async fn dj_status(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let enabled = settings.get(&format!("dj_enabled_{zone_id}"))
        .ok().flatten().map(|v| v == "true").unwrap_or(false);
    Json(json!({
        "zone_id": zone_id,
        "dj_mode": enabled,
        "deck_a": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "deck_b": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "crossfader": 0.5,
        "auto_crossfade": false,
    }))
}

#[derive(Deserialize)]
struct DjPlayRequest {
    zone_id: i64,
}

async fn dj_play(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": true}))
}

async fn dj_pause(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": false}))
}

#[derive(Deserialize)]
struct CrossfadeRequest {
    zone_id: i64,
    duration_ms: Option<i64>,
}

async fn dj_crossfade(Json(body): Json<CrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfade_started": true,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct CrossfaderRequest {
    zone_id: i64,
    position: f64,
}

async fn dj_crossfader(Json(body): Json<CrossfaderRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfader": body.position.clamp(0.0, 1.0),
    }))
}

#[derive(Deserialize)]
struct AutoCrossfadeRequest {
    zone_id: i64,
    enabled: bool,
    duration_ms: Option<i64>,
}

async fn dj_auto_crossfade(Json(body): Json<AutoCrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "auto_crossfade": body.enabled,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct LoadDeckRequest {
    track_id: i64,
}

async fn dj_load(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<LoadDeckRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "track_id": body.track_id,
        "loaded": true,
    }))
}

#[derive(Deserialize)]
struct DeckVolumeRequest {
    volume: f64,
}

async fn dj_volume(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<DeckVolumeRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "volume": body.volume.clamp(0.0, 1.0),
    }))
}

async fn dj_sync_tempo(Path(zone_id): Path<i64>) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "synced": true,
        "message": "tempo sync not yet implemented",
    }))
}

async fn dj_waveform(Path(track_id): Path<i64>) -> impl IntoResponse {
    Json(json!({
        "track_id": track_id,
        "waveform": null,
        "message": "waveform generation not yet implemented",
    }))
}

async fn dj_analyze(Path(track_id): Path<i64>) -> impl IntoResponse {
    Json(json!({
        "track_id": track_id,
        "bpm": null,
        "key": null,
        "message": "audio analysis not yet implemented",
    }))
}
