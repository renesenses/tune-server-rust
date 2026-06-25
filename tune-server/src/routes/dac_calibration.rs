use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::dac_calibration::{self, DacProfile};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::room_correction::FilterType;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/community", get(list_community_handler))
        .route("/community/search", get(search_community_handler))
        .route("/community/submit", post(submit_community_handler))
        .route("/community/{slug}", get(get_community_handler))
        .route(
            "/zones/{zone_id}",
            get(get_zone_profile_handler).delete(remove_zone_profile_handler),
        )
        .route("/zones/{zone_id}/apply", post(apply_profile_handler))
}

// ---------------------------------------------------------------------------
// Community endpoints
// ---------------------------------------------------------------------------

/// `GET /dac-calibration/community` — list all community DAC profiles.
async fn list_community_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    let profiles = dac_calibration::list_community_profiles(&state.http_client)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(json!({
        "profiles": profiles,
        "count": profiles.len(),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

/// `GET /dac-calibration/community/search?q=topping` — search profiles.
async fn search_community_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    let profiles = dac_calibration::search_profiles(&state.http_client, &params.q)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(json!({
        "query": params.q,
        "profiles": profiles,
        "count": profiles.len(),
    }))
    .into_response())
}

/// `GET /dac-calibration/community/{slug}` — get a single profile.
async fn get_community_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    let profile = dac_calibration::get_profile(&state.http_client, &slug)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(json!(profile)).into_response())
}

// ---------------------------------------------------------------------------
// Submit endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SubmitBody {
    profile: DacProfile,
}

/// `POST /dac-calibration/community/submit` — contribute a profile.
async fn submit_community_handler(
    State(state): State<AppState>,
    Json(body): Json<SubmitBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    let instance_id = tune_core::license::LicenseManager::hardware_fingerprint();

    dac_calibration::submit_profile(&state.http_client, &body.profile, &instance_id)
        .await
        .map_err(AppError::internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "submitted": true,
            "slug": body.profile.slug,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Zone-level endpoints
// ---------------------------------------------------------------------------

/// `GET /dac-calibration/zones/{zone_id}` — get applied DAC profile.
async fn get_zone_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    match dac_calibration::load_local_profile(&state.backend, &zone_id) {
        Some(profile) => Ok(Json(json!(profile)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no DAC profile applied to this zone"})),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct ApplyBody {
    profile: DacProfile,
}

/// `POST /dac-calibration/zones/{zone_id}/apply` — apply a DAC profile to a
/// zone: stores it locally and writes correction filters to the zone's EQ.
async fn apply_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
    Json(body): Json<ApplyBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    // Persist the profile locally.
    dac_calibration::save_local_profile(&state.backend, &zone_id, &body.profile)
        .map_err(AppError::internal)?;

    // Apply correction filters to the zone's EQ (same key the playback DSP
    // reads, consistent with room_correction).
    if !body.profile.corrections.is_empty() {
        let bands: Vec<serde_json::Value> = body
            .profile
            .corrections
            .iter()
            .map(|f| {
                json!({
                    "freq": f.frequency_hz,
                    "gain": f.gain_db,
                    "q": f.q_factor,
                    "type": match f.filter_type {
                        FilterType::Peaking => "peak",
                        FilterType::LowShelf => "low_shelf",
                        FilterType::HighShelf => "high_shelf",
                        FilterType::Notch => "notch",
                    },
                })
            })
            .collect();

        let eq_profile = json!({
            "enabled": true,
            "source": "dac_calibration",
            "profile_name": format!("{} {}", body.profile.manufacturer, body.profile.model),
            "bands": bands,
            "preamp_db": 0.0,
        });

        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&eq_profile)
                    .map_err(|e| AppError::internal(e.to_string()))?,
            )
            .map_err(AppError::internal)?;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "applied": true,
            "zone_id": zone_id,
            "slug": body.profile.slug,
            "filter_count": body.profile.corrections.len(),
        })),
    )
        .into_response())
}

/// `DELETE /dac-calibration/zones/{zone_id}` — remove applied profile.
async fn remove_zone_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::DacCalibration).await
    {
        return Ok(resp);
    }

    let existed = dac_calibration::delete_local_profile(&state.backend, &zone_id)
        .map_err(AppError::internal)?;

    if existed {
        // Clear the EQ profile that was written by apply.
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings.delete(&format!("zone_{zone_id}_eq_profile")).ok();

        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no DAC profile applied to this zone"})),
        )
            .into_response())
    }
}
