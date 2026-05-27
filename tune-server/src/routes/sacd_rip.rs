use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(sacd_status))
        .route("/disc", get(sacd_disc_info))
        .route("/rip", post(start_sacd_rip))
        .route("/rip/status", get(sacd_rip_status))
}

/// Check if sacd_extract or similar tool is available.
async fn sacd_status() -> Json<Value> {
    let sacd_extract = tokio::process::Command::new("which")
        .arg("sacd_extract")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    Json(json!({
        "available": sacd_extract,
        "tool": if sacd_extract { "sacd_extract" } else { "none" },
        "message": if sacd_extract {
            "sacd_extract found"
        } else {
            "SACD ripping requires sacd_extract and specialized hardware (compatible Blu-ray drive with SACD support)"
        },
        "requirements": [
            "Compatible Blu-ray/SACD drive",
            "sacd_extract binary installed",
            "Physical SACD disc inserted",
        ],
    }))
}

/// Read SACD disc information (stub — requires hardware).
async fn sacd_disc_info() -> Json<Value> {
    Json(json!({
        "disc_detected": false,
        "title": null,
        "artist": null,
        "tracks_stereo": [],
        "tracks_multichannel": [],
        "layers": {
            "stereo": false,
            "multichannel": false,
            "cd_layer": false,
        },
        "message": "SACD disc info requires compatible hardware. Insert a disc and ensure sacd_extract is available.",
    }))
}

#[derive(Deserialize)]
struct SacdRipRequest {
    /// Output directory
    output_dir: Option<String>,
    /// Output format: "dsf" (DSD), "dff" (DSD), "iso"
    format: Option<String>,
    /// Layer to rip: "stereo", "multichannel", or "both"
    layer: Option<String>,
}

/// Start an SACD rip (stub — requires hardware).
async fn start_sacd_rip(
    State(state): State<AppState>,
    Json(body): Json<SacdRipRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());

    let output_dir = body
        .output_dir
        .or_else(|| settings.get("sacd_rip_output_dir").ok().flatten())
        .unwrap_or_else(|| "/tmp/tune-sacd-rip".into());
    let format = body.format.unwrap_or_else(|| "dsf".into());
    let layer = body.layer.unwrap_or_else(|| "stereo".into());

    let rip_id = uuid::Uuid::new_v4().to_string();

    let rip_state = json!({
        "id": rip_id,
        "status": "not_available",
        "output_dir": output_dir,
        "format": format,
        "layer": layer,
        "progress": 0,
        "message": "SACD ripping requires compatible hardware. This is a stub implementation.",
    });

    settings
        .set("sacd_rip_current", &serde_json::to_string(&rip_state).unwrap())
        .ok();

    Json(rip_state)
}

/// Get current SACD rip status.
async fn sacd_rip_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let current = settings
        .get("sacd_rip_current")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    match current {
        Some(rip) => Json(rip),
        None => Json(json!({
            "status": "idle",
            "message": "No SACD rip in progress",
        })),
    }
}
