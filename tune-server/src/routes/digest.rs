use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::digest;
use tune_core::license::Feature;

use crate::error::AppError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/preview", get(preview))
        .route("/send", post(send))
        .route("/settings", get(get_settings).post(save_settings))
}

// ---------------------------------------------------------------------------
// GET /digest/preview — generate and return the digest as JSON
// ---------------------------------------------------------------------------

async fn preview(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::WeeklyDigest).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "weekly_digest",
        })));
    }

    let backend = state.backend.clone();
    let report = tokio::task::spawn_blocking(move || digest::generate_digest(&backend))
        .await
        .map_err(|e| AppError::internal(format!("digest task: {e}")))?
        .map_err(|e| AppError::internal(e))?;

    Ok(Json(json!(report)))
}

// ---------------------------------------------------------------------------
// POST /digest/send — generate digest + push to mozaiklabs.fr for emailing
// ---------------------------------------------------------------------------

async fn send(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::WeeklyDigest).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "weekly_digest",
        })));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Check that digest is enabled and we have an email
    let enabled = settings
        .get("digest_enabled")
        .ok()
        .flatten()
        .unwrap_or_default();
    if enabled != "true" {
        return Ok(Json(json!({
            "error": "digest_not_enabled",
            "message": "Enable weekly digest in settings first",
        })));
    }

    let email = settings
        .get("digest_email")
        .ok()
        .flatten()
        .unwrap_or_default();
    if email.is_empty() {
        return Ok(Json(json!({
            "error": "digest_no_email",
            "message": "Set an email address in digest settings first",
        })));
    }

    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    // Generate the report
    let backend = state.backend.clone();
    let report = tokio::task::spawn_blocking(move || digest::generate_digest(&backend))
        .await
        .map_err(|e| AppError::internal(format!("digest task: {e}")))?
        .map_err(|e| AppError::internal(e))?;

    // Push to mozaiklabs.fr API
    let body = json!({
        "instance_id": instance_id,
        "email": email,
        "report": report,
    });

    let resp = state
        .http_client
        .post("https://mozaiklabs.fr/api/v1/digest/send")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("digest send: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        warn!(status = %status, body = %text, "digest_send_failed");
        return Ok(Json(json!({
            "error": "digest_send_failed",
            "status": status.as_u16(),
            "detail": text,
        })));
    }

    info!(email = %email, "digest_sent");
    Ok(Json(json!({
        "ok": true,
        "email": email,
        "report": report,
    })))
}

// ---------------------------------------------------------------------------
// GET /digest/settings — read preferences
// ---------------------------------------------------------------------------

async fn get_settings(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::WeeklyDigest).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "weekly_digest",
        })));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let enabled = settings
        .get("digest_enabled")
        .ok()
        .flatten()
        .unwrap_or_else(|| "false".to_string());
    let day_of_week = settings
        .get("digest_day_of_week")
        .ok()
        .flatten()
        .unwrap_or_else(|| "monday".to_string());
    let email = settings
        .get("digest_email")
        .ok()
        .flatten()
        .unwrap_or_default();

    Ok(Json(json!({
        "enabled": enabled == "true",
        "day_of_week": day_of_week,
        "email": email,
    })))
}

// ---------------------------------------------------------------------------
// POST /digest/settings — save preferences
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DigestSettings {
    enabled: Option<bool>,
    day_of_week: Option<String>,
    email: Option<String>,
}

async fn save_settings(
    State(state): State<AppState>,
    Json(body): Json<DigestSettings>,
) -> Result<Json<Value>, AppError> {
    if !state.license.check_feature(Feature::WeeklyDigest).await {
        return Ok(Json(json!({
            "error": "premium_required",
            "feature": "weekly_digest",
        })));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    if let Some(enabled) = body.enabled {
        settings
            .set("digest_enabled", if enabled { "true" } else { "false" })
            .map_err(|e| AppError::internal(e))?;
    }
    if let Some(ref day) = body.day_of_week {
        settings
            .set("digest_day_of_week", day)
            .map_err(|e| AppError::internal(e))?;
    }
    if let Some(ref email) = body.email {
        settings
            .set("digest_email", email)
            .map_err(|e| AppError::internal(e))?;
    }

    info!(
        enabled = ?body.enabled,
        day = ?body.day_of_week,
        email = ?body.email,
        "digest_settings_saved"
    );

    Ok(Json(json!({
        "ok": true,
        "enabled": body.enabled,
        "day_of_week": body.day_of_week,
        "email": body.email,
    })))
}
