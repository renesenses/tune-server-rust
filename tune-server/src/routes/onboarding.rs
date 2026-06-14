use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const STEPS: &[&str] = &[
    "welcome",
    "music-dirs",
    "streaming",
    "zones",
    "profile",
    "complete",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(onboarding_status))
        .route("/step/welcome", post(step_welcome))
        .route("/step/music-dirs", post(step_music_dirs))
        .route("/step/streaming", post(step_streaming))
        .route("/step/zones", post(step_zones))
        .route("/step/profile", post(step_profile))
        .route("/step/complete", post(step_complete))
        .route("/skip", post(skip_onboarding))
}

/// Check if onboarding is complete.
async fn onboarding_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let complete = settings
        .get("onboarding_complete")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let current_step: i64 = settings
        .get("onboarding_step")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let steps: Vec<Value> = STEPS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let idx = i as i64;
            json!({
                "index": idx,
                "name": name,
                "done": idx < current_step,
                "current": idx == current_step,
            })
        })
        .collect();

    Json(json!({
        "complete": complete,
        "current_step": current_step,
        "steps": steps,
    }))
}

/// Step 1: Welcome - marks step 1 done.
async fn step_welcome(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    advance_step(&settings, 1);
    Json(json!({
        "step": "welcome",
        "status": "done",
        "next": "music-dirs",
    }))
}

#[derive(Deserialize)]
struct MusicDirsBody {
    dirs: Vec<String>,
}

/// Step 2: Configure music directories and trigger first scan.
async fn step_music_dirs(
    State(state): State<AppState>,
    Json(body): Json<MusicDirsBody>,
) -> impl IntoResponse {
    if body.dirs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "At least one music directory is required"})),
        )
            .into_response();
    }

    // Validate directories exist (normalizing paths for Windows compatibility)
    let mut valid_dirs = Vec::new();
    let mut invalid_dirs = Vec::new();
    for dir in &body.dirs {
        let normalized = tune_core::scanner::walker::normalize_path(dir);
        if std::path::Path::new(&normalized).is_dir() {
            valid_dirs.push(normalized);
        } else {
            invalid_dirs.push(dir.clone());
        }
    }

    if valid_dirs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "No valid directories found",
                "invalid_dirs": invalid_dirs,
            })),
        )
            .into_response();
    }

    let settings = SettingsRepo::new(state.db.clone());
    let dirs_json = serde_json::to_string(&valid_dirs).unwrap_or_else(|_| "[]".into());
    settings.set("music_dirs", &dirs_json).ok();
    advance_step(&settings, 2);

    // Trigger library scan via the config (the scan system watches for music_dirs changes)
    Json(json!({
        "step": "music-dirs",
        "status": "done",
        "next": "streaming",
        "valid_dirs": valid_dirs,
        "invalid_dirs": invalid_dirs,
        "scan_triggered": true,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct StreamingBody {
    service: String,
    credentials: Option<Value>,
}

/// Step 3: Authenticate streaming service.
async fn step_streaming(
    State(state): State<AppState>,
    Json(body): Json<StreamingBody>,
) -> impl IntoResponse {
    let valid_services = ["tidal", "qobuz", "spotify", "deezer"];
    if !valid_services.contains(&body.service.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Unknown service: {}", body.service),
                "valid_services": valid_services,
            })),
        )
            .into_response();
    }

    let settings = SettingsRepo::new(state.db.clone());

    // Store credentials if provided (for services that use token auth)
    if let Some(creds) = &body.credentials {
        if let Some(obj) = creds.as_object() {
            for (key, value) in obj {
                let skey = format!("{}_{}", body.service, key);
                if let Some(sval) = value.as_str() {
                    settings.set(&skey, sval).ok();
                }
            }
        }
    }

    // Store which streaming service was configured during onboarding
    settings
        .set("onboarding_streaming_service", &body.service)
        .ok();
    advance_step(&settings, 3);

    // Return auth URL for OAuth-based services
    let auth_info = match body.service.as_str() {
        "tidal" | "spotify" => json!({
            "auth_type": "oauth",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        "qobuz" => json!({
            "auth_type": "login_password",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        "deezer" => json!({
            "auth_type": "arl_token",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        _ => json!({"auth_type": "unknown"}),
    };

    Json(json!({
        "step": "streaming",
        "status": "done",
        "next": "zones",
        "service": body.service,
        "auth": auth_info,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ZonesBody {
    auto_discover: Option<bool>,
}

/// Step 4: Trigger device scan and create zones for discovered DLNA devices.
async fn step_zones(
    State(state): State<AppState>,
    Json(body): Json<ZonesBody>,
) -> impl IntoResponse {
    let auto_discover = body.auto_discover.unwrap_or(true);
    let settings = SettingsRepo::new(state.db.clone());

    if auto_discover {
        // Trigger SSDP discovery
        let scanner = state.scanner.lock().await;
        let discovered = scanner.rescan().await;
        tracing::info!(count = discovered.len(), "onboarding_zone_discovery");
        drop(scanner);
    }

    settings
        .set(
            "onboarding_auto_discover",
            if auto_discover { "true" } else { "false" },
        )
        .ok();
    advance_step(&settings, 4);

    Json(json!({
        "step": "zones",
        "status": "done",
        "next": "profile",
        "auto_discover": auto_discover,
        "discovery_started": auto_discover,
        "zones_url": "/api/v1/zones",
        "devices_url": "/api/v1/devices",
    }))
}

#[derive(Deserialize)]
struct ProfileBody {
    name: String,
    avatar_color: Option<String>,
}

/// Step 5: Create first user profile.
async fn step_profile(
    State(state): State<AppState>,
    Json(body): Json<ProfileBody>,
) -> impl IntoResponse {
    if body.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Profile name is required"})),
        )
            .into_response();
    }

    let settings = SettingsRepo::new(state.db.clone());

    // Create profile via profile_repo
    let profile_repo = tune_core::db::profile_repo::ProfileRepo::new(state.db.clone());
    let display_name = body.name.clone();
    let avatar_color = body.avatar_color.as_deref().unwrap_or("#6366f1");
    match profile_repo.create(&display_name, Some(&display_name), Some(avatar_color)) {
        Ok(profile_id) => {
            // Set as active profile
            settings
                .set("active_profile_id", &profile_id.to_string())
                .ok();
            advance_step(&settings, 5);

            Json(json!({
                "step": "profile",
                "status": "done",
                "next": "complete",
                "profile_id": profile_id,
                "name": display_name,
                "avatar_color": avatar_color,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "onboarding_profile_create_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create profile: {e}")})),
            )
                .into_response()
        }
    }
}

/// Step 6: Mark onboarding complete. Returns summary.
async fn step_complete(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("onboarding_complete", "true").ok();
    settings
        .set("onboarding_step", &STEPS.len().to_string())
        .ok();

    // Build summary
    let music_dirs = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".into());
    let streaming_service = settings
        .get("onboarding_streaming_service")
        .ok()
        .flatten()
        .unwrap_or_default();
    let auto_discover = settings
        .get("onboarding_auto_discover")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let active_profile_id = settings
        .get("active_profile_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    Json(json!({
        "step": "complete",
        "status": "done",
        "complete": true,
        "summary": {
            "music_dirs": serde_json::from_str::<Value>(&music_dirs).unwrap_or(json!([])),
            "streaming_service": streaming_service,
            "auto_discover": auto_discover,
            "active_profile_id": active_profile_id,
        },
    }))
}

/// Skip all steps, mark onboarding complete.
async fn skip_onboarding(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("onboarding_complete", "true").ok();
    settings
        .set("onboarding_step", &STEPS.len().to_string())
        .ok();

    Json(json!({
        "skipped": true,
        "complete": true,
    }))
}

fn advance_step(settings: &SettingsRepo, step: i64) {
    let current: i64 = settings
        .get("onboarding_step")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if step > current {
        settings.set("onboarding_step", &step.to_string()).ok();
    }
}
