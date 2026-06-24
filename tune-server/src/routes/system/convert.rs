use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct ConvertRequest {
    pub track_id: i64,
    pub target_format: String,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
}

pub(super) async fn convert_track(
    State(state): State<AppState>,
    Json(body): Json<ConvertRequest>,
) -> impl IntoResponse {
    // 1. Get track file_path from DB
    let track = TrackRepo::with_backend(state.backend.clone())
        .get(body.track_id)
        .ok()
        .flatten();

    let track = match track {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response();
        }
    };

    let file_path = match track.file_path {
        Some(ref p) => p.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no file path"})),
            )
                .into_response();
        }
    };

    // 2. Read the file
    let file_bytes = match tokio::fs::read(&file_path).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("read: {e}")})),
            )
                .into_response();
        }
    };

    // 3. Get instance_id from settings
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 4. Upload to mozaiklabs.fr
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "track.flac".to_string());

    let mut form = reqwest::multipart::Form::new()
        .text("target_format", body.target_format.clone())
        .text("instance_id", instance_id)
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_bytes)
                .file_name(file_name)
                .mime_str("application/octet-stream")
                .unwrap(),
        );

    if let Some(sr) = body.sample_rate {
        form = form.text("sample_rate", sr.to_string());
    }
    if let Some(bd) = body.bit_depth {
        form = form.text("bit_depth", bd.to_string());
    }

    let resp = state
        .http_client
        .post("https://mozaiklabs.fr/api/v1/premium/convert")
        .multipart(form)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let data: Value = r.json().await.unwrap_or(json!({"error": "parse failed"}));
            Json(data).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("cloud: {} {}", status, body)})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("cloud: {e}")})),
        )
            .into_response(),
    }
}

pub(super) async fn convert_status(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let resp = state
        .http_client
        .get(format!(
            "https://mozaiklabs.fr/api/v1/premium/convert/{job_id}"
        ))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let data: Value = r.json().await.unwrap_or(json!({"error": "parse"}));
            Json(data).into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("HTTP {}", r.status())})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("{e}")})),
        )
            .into_response(),
    }
}

pub(super) async fn convert_download(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let resp = state
        .http_client
        .get(format!(
            "https://mozaiklabs.fr/api/v1/premium/convert/{job_id}/download"
        ))
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let headers = r.headers().clone();
            let bytes = r.bytes().await.unwrap_or_default();
            let mut response = axum::response::Response::new(axum::body::Body::from(bytes));
            if let Some(ct) = headers.get("content-type") {
                response.headers_mut().insert("content-type", ct.clone());
            }
            if let Some(cd) = headers.get("content-disposition") {
                response
                    .headers_mut()
                    .insert("content-disposition", cd.clone());
            }
            response.into_response()
        }
        _ => (StatusCode::BAD_GATEWAY, "download failed").into_response(),
    }
}
