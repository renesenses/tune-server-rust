use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::audio::decode::decode_to_pcm;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

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

async fn enable_dj(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set(&format!("dj_enabled_{zone_id}"), "true").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": true}))
}

async fn disable_dj(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set(&format!("dj_enabled_{zone_id}"), "false").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": false}))
}

async fn dj_status(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let enabled = settings
        .get(&format!("dj_enabled_{zone_id}"))
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
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

async fn dj_waveform(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively, then downsample to ~8kHz equivalent by striding
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples.is_empty() => {
            let source_rate = audio.sample_rate as usize;
            // Stride factor to approximate 8kHz from native rate
            let stride = (source_rate / 8000).max(1);
            let samples: Vec<f32> = audio
                .samples
                .iter()
                .step_by(stride)
                .map(|&s| s as f32 / 32768.0)
                .collect();

            // Downsample to ~200 points (peak amplitude per chunk)
            let target_points = 200usize;
            let chunk_size = (samples.len() / target_points).max(1);
            let waveform: Vec<f32> = samples
                .chunks(chunk_size)
                .map(|chunk| chunk.iter().map(|s| s.abs()).fold(0.0f32, f32::max))
                .collect();

            Json(json!({
                "track_id": track_id,
                "points": waveform.len(),
                "waveform": waveform,
            }))
            .into_response()
        }
        _ => Json(json!({
            "track_id": track_id,
            "waveform": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}

async fn dj_analyze(State(state): State<AppState>, Path(track_id): Path<i64>) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively for energy-based beat detection
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples.is_empty() => {
            let source_rate = audio.sample_rate as usize;
            // Stride to approximate 22050 Hz from native rate
            let stride = (source_rate / 22050).max(1);
            let effective_rate: usize = source_rate / stride;

            let samples: Vec<f32> = audio
                .samples
                .iter()
                .step_by(stride)
                .map(|&s| s as f32 / 32768.0)
                .collect();

            // 250 ms windows for energy computation
            let window_size = effective_rate / 4;
            if window_size == 0 {
                return Json(json!({
                    "track_id": track_id,
                    "bpm": null,
                    "error": "audio too short for analysis",
                }))
                .into_response();
            }

            let energies: Vec<f32> = samples
                .chunks(window_size)
                .map(|chunk| chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32)
                .collect();

            if energies.len() < 4 {
                return Json(json!({
                    "track_id": track_id,
                    "bpm": null,
                    "error": "audio too short for analysis",
                }))
                .into_response();
            }

            let avg_energy: f32 = energies.iter().sum::<f32>() / energies.len() as f32;
            let threshold = avg_energy * 1.3;

            // Count onset peaks (energy crossing above threshold)
            let mut beats = 0u32;
            let mut prev_above = false;
            for &e in &energies {
                let above = e > threshold;
                if above && !prev_above {
                    beats += 1;
                }
                prev_above = above;
            }

            let duration_secs = samples.len() as f64 / effective_rate as f64;
            let bpm_raw = if duration_secs > 0.0 {
                (beats as f64 / duration_secs * 60.0).round()
            } else {
                0.0
            };
            // Only report BPM in plausible range
            let bpm = if (60.0..=200.0).contains(&bpm_raw) {
                Some(bpm_raw)
            } else {
                None
            };

            Json(json!({
                "track_id": track_id,
                "bpm": bpm,
                "duration_s": duration_secs.round(),
                "beats_detected": beats,
            }))
            .into_response()
        }
        _ => Json(json!({
            "track_id": track_id,
            "bpm": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}
