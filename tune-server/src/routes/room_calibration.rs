use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(cal_status))
        .route("/profiles", get(list_cal_profiles).post(create_cal_profile))
        .route("/profiles/{id}", get(get_cal_profile).delete(delete_cal_profile))
        .route("/profiles/{id}/activate", post(activate_cal_profile))
        .route("/measure", post(start_measurement))
        .route("/measure/status", get(measurement_status))
}

fn load_profiles(state: &AppState) -> Vec<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("room_cal_profiles")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_profiles(state: &AppState, profiles: &[Value]) {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .set("room_cal_profiles", &serde_json::to_string(profiles).unwrap())
        .ok();
}

/// Status of room calibration subsystem.
async fn cal_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let profiles = load_profiles(&state);
    let active_id = settings.get("room_cal_active_profile").ok().flatten();

    let active_profile = active_id.as_ref().and_then(|id| {
        profiles.iter().find(|p| p["id"].as_str() == Some(id))
    });

    Json(json!({
        "available": true,
        "profile_count": profiles.len(),
        "active_profile_id": active_id,
        "active_profile_name": active_profile.and_then(|p| p["name"].as_str()),
        "microphone_required": true,
        "message": "Room calibration profiles can be managed. Measurement requires microphone input.",
    }))
}

/// List all calibration profiles.
async fn list_cal_profiles(State(state): State<AppState>) -> Json<Value> {
    let profiles = load_profiles(&state);
    let settings = SettingsRepo::new(state.db.clone());
    let active_id = settings.get("room_cal_active_profile").ok().flatten();

    Json(json!({
        "profiles": profiles,
        "active_profile_id": active_id,
    }))
}

#[derive(Deserialize)]
struct CreateProfileBody {
    name: String,
    /// Room name/description
    room: Option<String>,
    /// Speaker position description
    speaker_position: Option<String>,
    /// Listening position description
    listening_position: Option<String>,
    /// EQ correction bands (optional, can be populated later by measurement)
    #[serde(default)]
    bands: Vec<Value>,
}

/// Create a new calibration profile.
async fn create_cal_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateProfileBody>,
) -> impl IntoResponse {
    let mut profiles = load_profiles(&state);
    let id = uuid::Uuid::new_v4().to_string();

    let profile = json!({
        "id": id,
        "name": body.name,
        "room": body.room,
        "speaker_position": body.speaker_position,
        "listening_position": body.listening_position,
        "bands": body.bands,
        "measured": false,
        "created_at": epoch_secs(),
    });

    profiles.push(profile.clone());
    save_profiles(&state, &profiles);

    (StatusCode::CREATED, Json(profile)).into_response()
}

/// Get a single calibration profile by ID.
async fn get_cal_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let profiles = load_profiles(&state);
    match profiles.iter().find(|p| p["id"].as_str() == Some(&id)) {
        Some(profile) => Json(profile.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response(),
    }
}

/// Delete a calibration profile.
async fn delete_cal_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut profiles = load_profiles(&state);
    let before = profiles.len();
    profiles.retain(|p| p["id"].as_str() != Some(&id));

    if profiles.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response();
    }

    save_profiles(&state, &profiles);

    // If this was the active profile, clear it
    let settings = SettingsRepo::new(state.db.clone());
    if settings
        .get("room_cal_active_profile")
        .ok()
        .flatten()
        .as_deref()
        == Some(&id)
    {
        settings.delete("room_cal_active_profile").ok();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Activate a calibration profile for use during playback.
async fn activate_cal_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let profiles = load_profiles(&state);
    let profile = profiles.iter().find(|p| p["id"].as_str() == Some(&id));

    match profile {
        Some(p) => {
            let settings = SettingsRepo::new(state.db.clone());
            settings.set("room_cal_active_profile", &id).ok();
            Json(json!({
                "active_profile_id": id,
                "active_profile_name": p["name"],
                "activated": true,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct MeasurementRequest {
    /// Profile ID to store results in
    profile_id: String,
    /// Duration of measurement sweep in seconds
    duration_secs: Option<u32>,
    /// Number of sweeps to average
    sweeps: Option<u32>,
}

/// Start a room measurement (stub — requires microphone input).
async fn start_measurement(
    State(state): State<AppState>,
    Json(body): Json<MeasurementRequest>,
) -> impl IntoResponse {
    let profiles = load_profiles(&state);
    let profile = profiles
        .iter()
        .find(|p| p["id"].as_str() == Some(&body.profile_id));

    if profile.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response();
    }

    let settings = SettingsRepo::new(state.db.clone());
    let measurement = json!({
        "status": "not_available",
        "profile_id": body.profile_id,
        "duration_secs": body.duration_secs.unwrap_or(10),
        "sweeps": body.sweeps.unwrap_or(3),
        "message": "Room measurement requires microphone input hardware. This is a stub.",
    });

    settings
        .set("room_cal_measurement", &serde_json::to_string(&measurement).unwrap())
        .ok();

    Json(measurement).into_response()
}

/// Get current measurement status.
async fn measurement_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let current = settings
        .get("room_cal_measurement")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    match current {
        Some(m) => Json(m),
        None => Json(json!({
            "status": "idle",
            "message": "No measurement in progress",
        })),
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
