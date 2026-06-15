use axum::extract::State;
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
        .route("/status", get(cd_status))
        .route("/drives", get(list_drives))
        .route("/disc", get(disc_info))
        .route("/rip", post(start_rip))
        .route("/rip/status", get(rip_status))
        .route("/rip/cancel", post(cancel_rip))
}

/// Check whether cdparanoia or cdda2wav is available on the system.
async fn cd_status() -> Json<Value> {
    let cdparanoia = tokio::process::Command::new("which")
        .arg("cdparanoia")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    let cdda2wav = tokio::process::Command::new("which")
        .arg("cdda2wav")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    // macOS: check diskutil availability
    let diskutil = tokio::process::Command::new("which")
        .arg("diskutil")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    let tool = if cdparanoia {
        "cdparanoia"
    } else if cdda2wav {
        "cdda2wav"
    } else {
        "none"
    };

    Json(json!({
        "available": cdparanoia || cdda2wav,
        "tool": tool,
        "cdparanoia": cdparanoia,
        "cdda2wav": cdda2wav,
        "diskutil": diskutil,
    }))
}

/// List CD/DVD drives. On Linux scan /dev/sr*, on macOS use diskutil.
async fn list_drives() -> Json<Value> {
    let mut drives = Vec::new();

    // Linux: check /dev/sr*
    if cfg!(target_os = "linux") {
        for i in 0..4 {
            let path = format!("/dev/sr{i}");
            if tokio::fs::metadata(&path).await.is_ok() {
                drives.push(json!({
                    "device": path,
                    "name": format!("CD Drive {i}"),
                }));
            }
        }
    }

    // macOS: list optical drives via diskutil
    if cfg!(target_os = "macos") {
        if let Ok(output) = tokio::process::Command::new("diskutil")
            .args(["list", "external"])
            .output()
            .await
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("/dev/disk") {
                        drives.push(json!({
                            "device": trimmed.split_whitespace().next().unwrap_or(trimmed),
                            "name": trimmed,
                        }));
                    }
                }
            }
        }
    }

    Json(json!({
        "drives": drives,
        "count": drives.len(),
    }))
}

/// Read Table of Contents from the CD using cdparanoia -Q.
async fn disc_info() -> impl IntoResponse {
    let result = tokio::process::Command::new("cdparanoia")
        .arg("-Q")
        .output()
        .await;

    match result {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stderr}{stdout}");

            // Parse track count from cdparanoia output
            let tracks: Vec<Value> = combined
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with(|c: char| c.is_ascii_digit()) && trimmed.contains('.')
                })
                .enumerate()
                .map(|(i, line)| {
                    json!({
                        "number": i + 1,
                        "raw": line.trim(),
                    })
                })
                .collect();

            Json(json!({
                "disc_detected": !tracks.is_empty(),
                "tracks": tracks,
                "track_count": tracks.len(),
                "raw_output": combined,
            }))
            .into_response()
        }
        Err(_) => Json(json!({
            "disc_detected": false,
            "tracks": [],
            "track_count": 0,
            "error": "cdparanoia not available or no disc inserted",
        }))
        .into_response(),
    }
}

#[derive(Deserialize)]
struct RipRequest {
    /// Output directory for ripped files
    output_dir: Option<String>,
    /// Audio format: "wav", "flac", "aiff"
    format: Option<String>,
    /// Specific tracks to rip (empty = all)
    #[serde(default)]
    tracks: Vec<u32>,
    /// CD drive device path
    device: Option<String>,
}

/// Start a background CD rip task.
async fn start_rip(
    State(state): State<AppState>,
    Json(body): Json<RipRequest>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    let output_dir = body
        .output_dir
        .or_else(|| settings.get("cd_rip_output_dir").ok().flatten())
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("tune-rip")
                .to_string_lossy()
                .to_string()
        });
    let format = body.format.unwrap_or_else(|| "wav".into());

    // Store rip state
    let rip_id = uuid::Uuid::new_v4().to_string();
    let rip_state = json!({
        "id": rip_id,
        "status": "running",
        "output_dir": output_dir,
        "format": format,
        "tracks": body.tracks,
        "device": body.device,
        "progress": 0,
        "started_at": chrono_now(),
    });

    settings
        .set("cd_rip_current", &serde_json::to_string(&rip_state)?)
        .ok();

    Ok(Json(json!({
        "id": rip_id,
        "status": "started",
        "output_dir": output_dir,
        "format": format,
        "message": "CD rip task queued. Poll /rip/status for progress.",
    })))
}

/// Get current rip progress.
async fn rip_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let current = settings
        .get("cd_rip_current")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    match current {
        Some(rip) => Json(rip),
        None => Json(json!({
            "status": "idle",
            "message": "No rip in progress",
        })),
    }
}

/// Cancel a running rip task.
async fn cancel_rip(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(current) = settings.get("cd_rip_current").ok().flatten() {
        if let Ok(mut rip) = serde_json::from_str::<Value>(&current) {
            rip["status"] = json!("cancelled");
            settings
                .set("cd_rip_current", &serde_json::to_string(&rip)?)
                .ok();
            return Ok(Json(json!({
                "status": "cancelled",
                "message": "Rip task cancelled",
            })));
        }
    }
    Ok(Json(json!({
        "status": "idle",
        "message": "No rip in progress to cancel",
    })))
}

fn chrono_now() -> String {
    // Simple ISO 8601 timestamp without chrono dependency
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}
