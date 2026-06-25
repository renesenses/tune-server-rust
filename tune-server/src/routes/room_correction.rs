use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::room_correction::{
    CorrectionFilter, FilterType, FrequencyPoint, RoomProfile, delete_profile,
    generate_correction_from_measurements, list_profiles, load_profile, save_profile,
};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/profiles", get(list_profiles_handler))
        .route(
            "/profiles/{zone_id}",
            get(get_profile_handler)
                .post(save_profile_handler)
                .delete(delete_profile_handler),
        )
        .route("/analyze", post(analyze_handler))
        .route("/profiles/{zone_id}/apply", post(apply_profile_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /room-correction/profiles` — list all room correction profiles.
async fn list_profiles_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profiles = list_profiles(&state.backend);
    Ok(Json(json!({
        "profiles": profiles,
        "count": profiles.len(),
    }))
    .into_response())
}

/// `GET /room-correction/profiles/{zone_id}` — get a zone's profile.
async fn get_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    match load_profile(&state.backend, &zone_id) {
        Some(profile) => Ok(Json(json!(profile)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response()),
    }
}

/// Request body for saving a room correction profile.
#[derive(Deserialize)]
struct SaveProfileBody {
    name: String,
    #[serde(default)]
    filters: Vec<CorrectionFilterInput>,
    /// Raw measurement data (JSON-encoded) for storage / re-analysis.
    measurement_data: Option<String>,
}

#[derive(Deserialize)]
struct CorrectionFilterInput {
    frequency_hz: f64,
    gain_db: f64,
    #[serde(default = "default_q")]
    q_factor: f64,
    #[serde(default = "default_filter_type")]
    filter_type: FilterType,
}

fn default_q() -> f64 {
    1.0
}

fn default_filter_type() -> FilterType {
    FilterType::Peaking
}

/// `POST /room-correction/profiles/{zone_id}` — save a profile.
async fn save_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
    Json(body): Json<SaveProfileBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let filters: Vec<CorrectionFilter> = body
        .filters
        .into_iter()
        .map(|f| CorrectionFilter {
            frequency_hz: f.frequency_hz,
            gain_db: f.gain_db,
            q_factor: f.q_factor,
            filter_type: f.filter_type,
        })
        .collect();

    let now = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simple ISO-8601 from epoch — good enough for a creation timestamp.
        let dt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        dt.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| format!("{secs}"))
    };
    let profile = RoomProfile {
        name: body.name,
        zone_id: zone_id.clone(),
        filters,
        created_at: now,
        measurement_data: body.measurement_data,
    };

    save_profile(&state.backend, &profile).map_err(AppError::internal)?;

    Ok((StatusCode::CREATED, Json(json!(profile))).into_response())
}

/// `DELETE /room-correction/profiles/{zone_id}` — delete a zone's profile.
async fn delete_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let existed = delete_profile(&state.backend, &zone_id).map_err(AppError::internal)?;
    if existed {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response())
    }
}

/// Request body for the analyze endpoint.
#[derive(Deserialize)]
struct AnalyzeBody {
    measurements: Vec<FrequencyPoint>,
}

/// `POST /room-correction/analyze` — analyze measurement data and return
/// suggested correction filters without saving anything.
async fn analyze_handler(
    State(state): State<AppState>,
    Json(body): Json<AnalyzeBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    if body.measurements.is_empty() {
        return Err(AppError::bad_request("measurements array is empty"));
    }

    let filters = generate_correction_from_measurements(&body.measurements);

    Ok(Json(json!({
        "filters": filters,
        "filter_count": filters.len(),
        "measurement_points": body.measurements.len(),
    }))
    .into_response())
}

/// `POST /room-correction/profiles/{zone_id}/apply` — apply a zone's room
/// correction profile to the zone's EQ (writes to the existing parametric
/// EQ settings key so the playback pipeline picks it up).
async fn apply_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profile = match load_profile(&state.backend, &zone_id) {
        Some(p) => p,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no profile for this zone"})),
            )
                .into_response());
        }
    };

    if profile.filters.is_empty() {
        return Ok(Json(json!({
            "applied": false,
            "zone_id": zone_id,
            "reason": "profile has no correction filters",
        }))
        .into_response());
    }

    // Convert correction filters to the EQ band format used by the existing
    // parametric EQ system (eq_pro / zone DSP).
    let bands: Vec<Value> = profile
        .filters
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

    // Write to the zone's EQ profile key (same key the playback DSP reads).
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let eq_profile = json!({
        "enabled": true,
        "source": "room_correction",
        "profile_name": profile.name,
        "bands": bands,
        "preamp_db": 0.0,
    });

    settings
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&eq_profile).map_err(|e| AppError::internal(e.to_string()))?,
        )
        .map_err(AppError::internal)?;

    Ok(Json(json!({
        "applied": true,
        "zone_id": zone_id,
        "profile_name": profile.name,
        "filter_count": profile.filters.len(),
    }))
    .into_response())
}
