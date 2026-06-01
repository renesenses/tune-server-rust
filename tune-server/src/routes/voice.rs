use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::state::AppState;

pub async fn voice_search(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let api_key = match state.config.openai_api_key.as_deref() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "TUNE_OPENAI_API_KEY not configured"})),
            )
                .into_response();
        }
    };

    let audio_base64 = match body["audio"].as_str() {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "audio field required (base64-encoded)"})),
            )
                .into_response();
        }
    };

    // Decode base64 audio
    let audio_bytes = match base64_decode(&audio_base64) {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid base64 audio"})),
            )
                .into_response();
        }
    };

    // Build multipart boundary manually (reqwest multipart feature not enabled)
    let boundary = "----TuneVoiceSearch";
    let mut body_bytes = Vec::new();
    body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
    body_bytes.extend_from_slice(&audio_bytes);
    body_bytes.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--{boundary}--\r\n").as_bytes());

    let resp = state
        .http_client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body_bytes)
        .send()
        .await;

    let transcription = match resp {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => v["text"].as_str().unwrap_or("").to_string(),
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("whisper parse: {e}")})),
                )
                    .into_response();
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("whisper: {e}")})),
            )
                .into_response();
        }
    };

    if transcription.is_empty() {
        return Json(json!({
            "transcription": "",
            "results": null,
            "error": "no speech detected",
        }))
        .into_response();
    }

    // Search library with transcribed text
    let db = state.db.clone();
    let track_repo = tune_core::db::track_repo::TrackRepo::new(db);
    let tracks = track_repo.search(&transcription, 10).unwrap_or_default();

    Json(json!({
        "transcription": transcription,
        "results": tracks.len(),
        "tracks": tracks.iter().map(|t| json!({
            "id": t.id,
            "title": t.title,
            "artist": t.artist_name,
            "album": t.album_title,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in input.as_bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' {
            continue;
        }
        let val = table.iter().position(|&c| c == b)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decode_valid() {
        let decoded = base64_decode("SGVsbG8=").unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn base64_decode_empty() {
        let decoded = base64_decode("").unwrap();
        assert!(decoded.is_empty());
    }
}
