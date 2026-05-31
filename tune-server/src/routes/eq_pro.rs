use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(eq_status))
        .route("/presets", get(list_presets).post(create_preset))
        .route(
            "/presets/{id}",
            get(get_preset).put(update_preset).delete(delete_preset),
        )
        .route("/presets/{id}/activate", post(activate_preset))
        .route("/bands", get(get_bands).post(set_bands))
        // Advanced EQ routes
        .route("/parametric", get(get_parametric).post(set_parametric))
        .route("/graphic", get(get_graphic).post(set_graphic))
        .route("/room-correction", post(apply_room_correction))
}

fn load_presets(state: &AppState) -> Vec<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("eq_presets")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_presets(state: &AppState, presets: &[Value]) -> Result<(), AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .set("eq_presets", &serde_json::to_string(presets)?)
        .ok();
    Ok(())
}

/// EQ subsystem status.
async fn eq_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let presets = load_presets(&state);
    let active_id = settings.get("eq_active_preset").ok().flatten();
    let enabled = settings
        .get("eq_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    let active_preset = active_id
        .as_ref()
        .and_then(|id| presets.iter().find(|p| p["id"].as_str() == Some(id)));

    Json(json!({
        "enabled": enabled,
        "preset_count": presets.len(),
        "active_preset_id": active_id,
        "active_preset_name": active_preset.and_then(|p| p["name"].as_str()),
        "supported_types": ["parametric", "graphic", "room_correction"],
        "max_bands": 31,
    }))
}

/// List all EQ presets.
async fn list_presets(State(state): State<AppState>) -> Json<Value> {
    let presets = load_presets(&state);
    let settings = SettingsRepo::new(state.db.clone());
    let active_id = settings.get("eq_active_preset").ok().flatten();

    Json(json!({
        "presets": presets,
        "active_preset_id": active_id,
    }))
}

#[derive(Deserialize)]
struct CreatePresetBody {
    name: String,
    #[serde(default)]
    bands: Vec<EqBand>,
    /// "parametric", "graphic", or "custom"
    eq_type: Option<String>,
    /// Zone ID this preset is for (None = global)
    zone_id: Option<String>,
}

#[derive(Deserialize, Clone)]
struct EqBand {
    /// Center frequency in Hz
    freq: f64,
    /// Gain in dB (-12 to +12 typical)
    gain: f64,
    /// Q factor (0.1 to 30)
    q: Option<f64>,
    /// Filter type: "peak", "low_shelf", "high_shelf", "low_pass", "high_pass", "notch"
    #[serde(rename = "type", default = "default_band_type")]
    band_type: String,
}

fn default_band_type() -> String {
    "peak".into()
}

impl EqBand {
    fn to_json(&self) -> Value {
        json!({
            "freq": self.freq,
            "gain": self.gain,
            "q": self.q.unwrap_or(1.0),
            "type": self.band_type,
        })
    }
}

/// Create a new EQ preset.
async fn create_preset(
    State(state): State<AppState>,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    let mut presets = load_presets(&state);
    let id = uuid::Uuid::new_v4().to_string();

    let bands_json: Vec<Value> = body.bands.iter().map(|b| b.to_json()).collect();

    let preset = json!({
        "id": id,
        "name": body.name,
        "eq_type": body.eq_type.unwrap_or_else(|| "parametric".into()),
        "zone_id": body.zone_id,
        "bands": bands_json,
        "created_at": epoch_secs(),
    });

    presets.push(preset.clone());
    save_presets(&state, &presets)?;

    Ok((StatusCode::CREATED, Json(preset)).into_response())
}

/// Get a single preset by ID.
async fn get_preset(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let presets = load_presets(&state);
    match presets.iter().find(|p| p["id"].as_str() == Some(&id)) {
        Some(preset) => Json(preset.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response(),
    }
}

/// Update an existing preset.
async fn update_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    let mut presets = load_presets(&state);
    let idx = presets.iter().position(|p| p["id"].as_str() == Some(&id));

    match idx {
        Some(i) => {
            let bands_json: Vec<Value> = body.bands.iter().map(|b| b.to_json()).collect();
            presets[i]["name"] = json!(body.name);
            presets[i]["bands"] = json!(bands_json);
            if let Some(t) = &body.eq_type {
                presets[i]["eq_type"] = json!(t);
            }
            if let Some(z) = &body.zone_id {
                presets[i]["zone_id"] = json!(z);
            }
            let updated = presets[i].clone();
            save_presets(&state, &presets)?;
            Ok(Json(updated).into_response())
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response()),
    }
}

/// Delete a preset.
async fn delete_preset(State(state): State<AppState>, Path(id): Path<String>) -> Result<impl IntoResponse, AppError> {
    let mut presets = load_presets(&state);
    let before = presets.len();
    presets.retain(|p| p["id"].as_str() != Some(&id));

    if presets.len() == before {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response());
    }

    save_presets(&state, &presets)?;

    let settings = SettingsRepo::new(state.db.clone());
    if settings.get("eq_active_preset").ok().flatten().as_deref() == Some(&id) {
        settings.delete("eq_active_preset").ok();
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Activate a preset for the current or specified zone.
async fn activate_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let presets = load_presets(&state);
    let preset = presets.iter().find(|p| p["id"].as_str() == Some(&id));

    match preset {
        Some(p) => {
            let settings = SettingsRepo::new(state.db.clone());
            settings.set("eq_active_preset", &id).ok();
            settings.set("eq_enabled", "true").ok();
            Json(json!({
                "active_preset_id": id,
                "active_preset_name": p["name"],
                "activated": true,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response(),
    }
}

/// Get current active EQ bands.
async fn get_bands(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let active_id = settings.get("eq_active_preset").ok().flatten();
    let presets = load_presets(&state);

    let bands = active_id
        .and_then(|id| {
            presets
                .iter()
                .find(|p| p["id"].as_str() == Some(&id))
                .and_then(|p| p["bands"].as_array())
                .cloned()
        })
        .unwrap_or_default();

    Json(json!({
        "bands": bands,
        "count": bands.len(),
        "active_preset_id": settings.get("eq_active_preset").ok().flatten(),
    }))
}

#[derive(Deserialize)]
struct SetBandsBody {
    bands: Vec<EqBand>,
}

/// Set EQ bands directly (updates active preset or creates a transient one).
async fn set_bands(State(state): State<AppState>, Json(body): Json<SetBandsBody>) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    let bands_json: Vec<Value> = body.bands.iter().map(|b| b.to_json()).collect();

    // If there's an active preset, update it; otherwise create a transient config
    if let Some(active_id) = settings.get("eq_active_preset").ok().flatten() {
        let mut presets = load_presets(&state);
        if let Some(p) = presets
            .iter_mut()
            .find(|p| p["id"].as_str() == Some(&active_id))
        {
            p["bands"] = json!(&bands_json);
            save_presets(&state, &presets)?;
        }
    }

    settings
        .set(
            "eq_current_bands",
            &serde_json::to_string(&bands_json)?,
        )
        .ok();

    Ok(Json(json!({
        "bands": bands_json,
        "count": bands_json.len(),
        "applied": true,
    })))
}

// --- Advanced EQ routes ---

/// Get parametric EQ state (multi-band with full control).
async fn get_parametric(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let parametric: Value = settings
        .get("eq_parametric")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({
            "enabled": false,
            "bands": [],
            "preamp_db": 0.0,
        }));
    Json(parametric)
}

#[derive(Deserialize)]
struct ParametricBody {
    enabled: Option<bool>,
    bands: Option<Vec<EqBand>>,
    preamp_db: Option<f64>,
}

/// Set parametric EQ configuration.
async fn set_parametric(
    State(state): State<AppState>,
    Json(body): Json<ParametricBody>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    let mut current: Value = settings
        .get("eq_parametric")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({"enabled": false, "bands": [], "preamp_db": 0.0}));

    if let Some(e) = body.enabled {
        current["enabled"] = json!(e);
    }
    if let Some(bands) = &body.bands {
        current["bands"] = json!(bands.iter().map(|b| b.to_json()).collect::<Vec<_>>());
    }
    if let Some(preamp) = body.preamp_db {
        current["preamp_db"] = json!(preamp);
    }

    settings
        .set("eq_parametric", &serde_json::to_string(&current)?)
        .ok();
    Ok(Json(json!({"saved": true, "parametric": current})))
}

/// Get graphic EQ state (fixed frequency bands).
async fn get_graphic(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let graphic: Value = settings
        .get("eq_graphic")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            // Default 31-band graphic EQ frequencies (ISO standard)
            let frequencies = [
                20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0,
                400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0,
                5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
            ];
            let bands: Vec<Value> = frequencies
                .iter()
                .map(|&f| json!({"freq": f, "gain": 0.0}))
                .collect();
            json!({
                "enabled": false,
                "bands": bands,
                "preamp_db": 0.0,
            })
        });
    Json(graphic)
}

#[derive(Deserialize)]
struct GraphicBody {
    enabled: Option<bool>,
    /// Array of {freq, gain} — must match the 31-band layout
    bands: Option<Vec<Value>>,
    preamp_db: Option<f64>,
}

/// Set graphic EQ bands.
async fn set_graphic(State(state): State<AppState>, Json(body): Json<GraphicBody>) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    let mut current: Value = settings
        .get("eq_graphic")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({"enabled": false, "bands": [], "preamp_db": 0.0}));

    if let Some(e) = body.enabled {
        current["enabled"] = json!(e);
    }
    if let Some(bands) = &body.bands {
        current["bands"] = json!(bands);
    }
    if let Some(preamp) = body.preamp_db {
        current["preamp_db"] = json!(preamp);
    }

    settings
        .set("eq_graphic", &serde_json::to_string(&current)?)
        .ok();
    Ok(Json(json!({"saved": true, "graphic": current})))
}

#[derive(Deserialize)]
struct RoomCorrectionBody {
    /// Path to a room correction impulse response file (WAV)
    impulse_response_path: Option<String>,
    /// Or calibration profile ID from room_calibration plugin
    calibration_profile_id: Option<String>,
}

/// Apply room correction EQ from a calibration profile or impulse response.
async fn apply_room_correction(
    State(state): State<AppState>,
    Json(body): Json<RoomCorrectionBody>,
) -> Result<impl IntoResponse, AppError> {
    let settings = SettingsRepo::new(state.db.clone());

    let correction = json!({
        "enabled": true,
        "impulse_response_path": body.impulse_response_path,
        "calibration_profile_id": body.calibration_profile_id,
        "applied_at": epoch_secs(),
        "message": "Room correction configuration saved. Actual convolution requires DSP pipeline integration.",
    });

    settings
        .set(
            "eq_room_correction",
            &serde_json::to_string(&correction)?,
        )
        .ok();

    Ok(Json(correction).into_response())
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
