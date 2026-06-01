use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/config", get(viz_config).post(set_viz_config))
        .route("/data", get(viz_data))
        .route("/spectrum", get(viz_spectrum))
        .route("/waveform", get(viz_waveform))
}

fn load_viz_config(state: &AppState) -> Value {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("visualizer_config")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({
            "enabled": false,
            "type": "spectrum",
            "bins": 32,
            "smoothing": 0.8,
            "min_db": -60,
            "max_db": 0,
            "color_scheme": "default",
        }))
}

/// Get visualizer configuration.
async fn viz_config(State(state): State<AppState>) -> Json<Value> {
    Json(load_viz_config(&state))
}

#[derive(Deserialize)]
struct VizConfigBody {
    enabled: Option<bool>,
    #[serde(rename = "type")]
    viz_type: Option<String>,
    bins: Option<u32>,
    smoothing: Option<f64>,
    min_db: Option<f64>,
    max_db: Option<f64>,
    color_scheme: Option<String>,
}

/// Update visualizer configuration.
async fn set_viz_config(
    State(state): State<AppState>,
    Json(body): Json<VizConfigBody>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    let mut config = load_viz_config(&state);
    let obj = config
        .as_object_mut()
        .ok_or_else(|| AppError::internal("visualizer config is not a JSON object"))?;

    if let Some(v) = body.enabled {
        obj.insert("enabled".into(), json!(v));
    }
    if let Some(v) = &body.viz_type {
        obj.insert("type".into(), json!(v));
    }
    if let Some(v) = body.bins {
        obj.insert("bins".into(), json!(v));
    }
    if let Some(v) = body.smoothing {
        obj.insert("smoothing".into(), json!(v));
    }
    if let Some(v) = body.min_db {
        obj.insert("min_db".into(), json!(v));
    }
    if let Some(v) = body.max_db {
        obj.insert("max_db".into(), json!(v));
    }
    if let Some(v) = &body.color_scheme {
        obj.insert("color_scheme".into(), json!(v));
    }

    settings
        .set("visualizer_config", &serde_json::to_string(&config)?)
        .ok();
    Ok(Json(json!({"saved": true, "config": config})))
}

/// Return mock spectrum data (32 frequency bins).
/// Real implementation would tap into the audio pipeline.
async fn viz_data(State(state): State<AppState>) -> Json<Value> {
    let config = load_viz_config(&state);
    let bins = config["bins"].as_u64().unwrap_or(32) as usize;

    // Generate mock spectrum data with a plausible shape:
    // bass-heavy rolloff with some mid presence
    let data: Vec<f64> = (0..bins)
        .map(|i| {
            let freq_ratio = i as f64 / bins as f64;
            // Simulated spectrum: bass emphasis, mid dip, treble rolloff
            let base = -10.0 - (freq_ratio * 40.0);
            let bass_bump = if freq_ratio < 0.15 { 8.0 } else { 0.0 };
            let mid_bump = if (0.3..0.5).contains(&freq_ratio) {
                3.0
            } else {
                0.0
            };
            (base + bass_bump + mid_bump).max(-60.0).min(0.0)
        })
        .collect();

    // Approximate frequency labels for 32 bins (20Hz to 20kHz, log scale)
    let freq_labels: Vec<String> = (0..bins)
        .map(|i| {
            let freq = 20.0 * (1000.0_f64).powf(i as f64 / bins as f64);
            if freq >= 1000.0 {
                format!("{:.1}k", freq / 1000.0)
            } else {
                format!("{:.0}", freq)
            }
        })
        .collect();

    Json(json!({
        "bins": bins,
        "data": data,
        "frequencies": freq_labels,
        "unit": "dB",
        "mock": true,
        "message": "Mock spectrum data — real data requires audio pipeline tap",
    }))
}

/// Return detailed spectrum analysis data.
async fn viz_spectrum(State(state): State<AppState>) -> Json<Value> {
    let config = load_viz_config(&state);
    let bins = config["bins"].as_u64().unwrap_or(32) as usize;

    let left: Vec<f64> = (0..bins)
        .map(|i| {
            let freq_ratio = i as f64 / bins as f64;
            (-10.0 - freq_ratio * 40.0 + if freq_ratio < 0.15 { 8.0 } else { 0.0 })
                .max(-60.0)
                .min(0.0)
        })
        .collect();

    let right: Vec<f64> = (0..bins)
        .map(|i| {
            let freq_ratio = i as f64 / bins as f64;
            (-12.0 - freq_ratio * 38.0 + if freq_ratio < 0.15 { 7.0 } else { 0.0 })
                .max(-60.0)
                .min(0.0)
        })
        .collect();

    Json(json!({
        "channels": {
            "left": left,
            "right": right,
        },
        "bins": bins,
        "sample_rate": 44100,
        "fft_size": 2048,
        "mock": true,
    }))
}

/// Return waveform data (amplitude over time).
async fn viz_waveform() -> Json<Value> {
    // Generate a mock waveform: 200 samples of normalized amplitude
    let samples: Vec<f64> = (0..200)
        .map(|i| {
            let t = i as f64 / 200.0;
            // Simulated audio waveform envelope
            let env = (t * std::f64::consts::PI * 4.0).sin().abs() * 0.7 + 0.1;
            (env * 100.0).round() / 100.0
        })
        .collect();

    Json(json!({
        "samples": samples,
        "sample_count": 200,
        "duration_ms": 5000,
        "peak": 0.8,
        "rms": 0.45,
        "mock": true,
        "message": "Mock waveform — real data requires audio pipeline tap",
    }))
}
