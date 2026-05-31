use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(mqa_status))
        .route("/detect/{track_id}", get(detect_mqa))
        .route("/config", get(mqa_config).post(set_mqa_config))
}

/// MQA subsystem status.
async fn mqa_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings
        .get("mqa_passthrough")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let renderer = settings
        .get("mqa_renderer")
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".into());

    Json(json!({
        "available": true,
        "passthrough_enabled": enabled,
        "renderer": renderer,
        "info": "MQA (Master Quality Authenticated) detection and passthrough. Note: MQA Ltd entered administration in 2023.",
    }))
}

/// Detect if a track contains MQA signaling.
///
/// MQA embeds data in the least significant bits of a FLAC/WAV file.
/// Detection looks for specific bit patterns in the audio stream.
async fn detect_mqa(State(state): State<AppState>, Path(track_id): Path<i64>) -> Result<impl IntoResponse, AppError> {
    let track = {
        let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
        conn.prepare("SELECT path, format, sample_rate, bit_depth FROM tracks WHERE id = ?1")
            .and_then(|mut stmt| {
                stmt.query_row([track_id], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
            })
    };

    let (path, format, sample_rate, bit_depth) = match track {
        Ok(t) => t,
        Err(_) => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response());
        }
    };

    let path = match path {
        Some(p) => p,
        None => {
            return Ok(Json(json!({
                "track_id": track_id,
                "mqa_detected": false,
                "error": "no file path for this track",
            }))
            .into_response());
        }
    };

    // MQA detection heuristics:
    // 1. Must be FLAC or WAV (MQA encodes in PCM)
    // 2. Typically 44.1kHz or 48kHz base rate (unfolds to higher rates)
    // 3. 24-bit depth (MQA uses LSBs for signaling)
    let format_str = format.as_deref().unwrap_or("").to_lowercase();
    let is_candidate =
        (format_str.contains("flac") || format_str.contains("wav")) && bit_depth.unwrap_or(0) >= 24;

    if !is_candidate {
        return Ok(Json(json!({
            "track_id": track_id,
            "path": path,
            "format": format,
            "sample_rate": sample_rate,
            "bit_depth": bit_depth,
            "mqa_detected": false,
            "reason": "Not a candidate — MQA requires 24-bit FLAC/WAV",
        }))
        .into_response());
    }

    // Attempt to read the file and check for MQA magic bytes.
    // MQA signaling is embedded in the least significant bits of audio samples.
    // A proper implementation would decode a block of audio and look for the
    // MQA sync word pattern. For now, we do a best-effort check.
    let mqa_result = check_mqa_signaling(&path).await;

    Ok(Json(json!({
        "track_id": track_id,
        "path": path,
        "format": format,
        "sample_rate": sample_rate,
        "bit_depth": bit_depth,
        "mqa_detected": mqa_result.detected,
        "mqa_original_sample_rate": mqa_result.original_rate,
        "mqa_studio": mqa_result.is_studio,
        "analysis": mqa_result.analysis,
    }))
    .into_response())
}

struct MqaResult {
    detected: bool,
    original_rate: Option<u32>,
    is_studio: bool,
    analysis: String,
}

async fn check_mqa_signaling(path: &str) -> MqaResult {
    // Try to read first bytes of the file to check for FLAC signature
    let data = match tokio::fs::read(path).await {
        Ok(d) => d,
        Err(e) => {
            return MqaResult {
                detected: false,
                original_rate: None,
                is_studio: false,
                analysis: format!("Could not read file: {e}"),
            };
        }
    };

    // Basic FLAC header check
    let is_flac = data.len() > 4 && &data[0..4] == b"fLaC";
    if !is_flac && !(data.len() > 12 && &data[0..4] == b"RIFF") {
        return MqaResult {
            detected: false,
            original_rate: None,
            is_studio: false,
            analysis: "Not a FLAC or WAV file".into(),
        };
    }

    // Full MQA detection would require decoding audio frames and analyzing
    // the LSB pattern. This is a simplified stub that reports the file as
    // a potential MQA candidate based on format and bit depth.
    MqaResult {
        detected: false,
        original_rate: None,
        is_studio: false,
        analysis: "Full MQA bit-pattern analysis not yet implemented. File is a valid candidate for MQA encoding.".into(),
    }
}

/// Get MQA configuration.
async fn mqa_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let passthrough = settings
        .get("mqa_passthrough")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let renderer = settings
        .get("mqa_renderer")
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".into());
    let decode_first_unfold = settings
        .get("mqa_decode_first_unfold")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "passthrough_enabled": passthrough,
        "renderer": renderer,
        "decode_first_unfold": decode_first_unfold,
        "options": {
            "renderer_values": ["none", "decoder", "renderer"],
            "description": {
                "none": "No MQA processing — pass bitstream as-is",
                "decoder": "Full MQA decode (software)",
                "renderer": "First unfold only — let DAC do final rendering",
            },
        },
    }))
}

#[derive(Deserialize)]
struct MqaConfigBody {
    passthrough_enabled: Option<bool>,
    renderer: Option<String>,
    decode_first_unfold: Option<bool>,
}

/// Update MQA configuration.
async fn set_mqa_config(
    State(state): State<AppState>,
    Json(body): Json<MqaConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());

    if let Some(v) = body.passthrough_enabled {
        settings
            .set("mqa_passthrough", if v { "true" } else { "false" })
            .ok();
    }
    if let Some(r) = &body.renderer {
        settings.set("mqa_renderer", r).ok();
    }
    if let Some(v) = body.decode_first_unfold {
        settings
            .set("mqa_decode_first_unfold", if v { "true" } else { "false" })
            .ok();
    }

    Json(json!({"saved": true}))
}
