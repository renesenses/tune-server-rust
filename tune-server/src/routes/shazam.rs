use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/recognize", post(recognize_audio))
        .route("/history", get(recognition_history))
}

#[derive(Deserialize)]
struct RecognizeBody {
    /// Base64-encoded audio data (PCM, WAV, or compressed)
    #[serde(default)]
    audio_data: Option<String>,
    /// Duration of audio in seconds
    duration_secs: Option<f64>,
    /// Sample rate of the audio
    sample_rate: Option<u32>,
}

/// Attempt to recognize audio content.
///
/// Real Shazam API requires enterprise access (Apple/Shazam).
/// Alternative: AudD API (https://audd.io/) or ACRCloud for song recognition.
async fn recognize_audio(
    State(state): State<AppState>,
    Json(body): Json<RecognizeBody>,
) -> impl IntoResponse {
    let has_audio = body
        .audio_data
        .as_ref()
        .map(|d| !d.is_empty())
        .unwrap_or(false);

    if !has_audio {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "audio_data required (base64-encoded audio)",
                "supported_formats": ["pcm_s16le", "wav", "mp3"],
            })),
        )
            .into_response();
    }

    // Check if an API key is configured for an audio recognition service
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let audd_token = settings.get("audd_api_token").ok().flatten();
    let acrcloud_key = settings.get("acrcloud_access_key").ok().flatten();

    if audd_token.is_some() || acrcloud_key.is_some() {
        // In production, send audio to the configured recognition API.
        // For now, return a stub response indicating the API is configured.
        let result = json!({
            "recognized": false,
            "service": if audd_token.is_some() { "audd" } else { "acrcloud" },
            "message": "Audio recognition API configured but full implementation pending. \
                        Audio data received and would be sent to the recognition service.",
            "audio_info": {
                "duration_secs": body.duration_secs,
                "sample_rate": body.sample_rate,
                "data_length": body.audio_data.as_ref().map(|d| d.len()),
            },
        });

        // Save to history
        save_to_history(&state, &result);

        return Json(result).into_response();
    }

    // No API configured
    let result = json!({
        "recognized": false,
        "message": "No audio recognition API configured. Set audd_api_token (audd.io) or acrcloud_access_key (ACRCloud) in settings.",
        "available_services": [
            {
                "name": "AudD",
                "settings_key": "audd_api_token",
                "url": "https://audd.io/",
                "pricing": "Free tier: 300 requests/day",
            },
            {
                "name": "ACRCloud",
                "settings_key": "acrcloud_access_key",
                "url": "https://www.acrcloud.com/",
                "pricing": "Free tier: 500 requests/day",
            },
        ],
    });

    Json(result).into_response()
}

/// Get recognition history.
async fn recognition_history(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let history: Vec<Value> = settings
        .get("shazam_history")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    Json(json!({
        "history": history,
        "count": history.len(),
    }))
}

fn save_to_history(state: &AppState, result: &Value) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut history: Vec<Value> = settings
        .get("shazam_history")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let entry = json!({
        "result": result,
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    });

    history.push(entry);

    // Keep last 100 entries
    if history.len() > 100 {
        history = history.split_off(history.len() - 100);
    }

    if let Ok(serialized) = serde_json::to_string(&history) {
        settings.set("shazam_history", &serialized).ok();
    }
}
