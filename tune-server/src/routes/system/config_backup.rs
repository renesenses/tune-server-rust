use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use tracing::{info, warn};

use tune_core::config_backup::ConfigSnapshot;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::state::AppState;

const CLOUD_API: &str = "https://mozaiklabs.fr/api/v1/premium/config-backup";

/// UTC timestamp as ISO-8601 string without chrono dependency.
fn utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: i64) -> (i64, i64, i64) {
    // Algorithm from Howard Hinnant
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as i64, d as i64)
}

/// Short timestamp for filenames (no chrono).
fn utc_filename_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

// ── GET /system/config-backup/export ────────────────────────────────

/// Export the full server configuration as a JSON download.
pub(super) async fn export(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    match tune_core::config_backup::export_config(&state.backend) {
        Ok(snapshot) => {
            let filename = format!("tune-config-{}.json", utc_filename_stamp());
            let json_bytes = serde_json::to_vec_pretty(&snapshot).unwrap_or_default();
            (
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "application/json".to_string(),
                    ),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                json_bytes,
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "config_backup_export_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
        }
    }
}

// ── POST /system/config-backup/import ───────────────────────────────

/// Import a configuration snapshot, merging with existing data.
pub(super) async fn import(
    State(state): State<AppState>,
    Json(snapshot): Json<ConfigSnapshot>,
) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    info!(
        version = %snapshot.version,
        zones = snapshot.zones.len(),
        settings = snapshot.settings.len(),
        playlists = snapshot.playlists.len(),
        "config_backup_import_started"
    );

    match tune_core::config_backup::import_config(&state.backend, snapshot) {
        Ok(report) => Json(json!({
            "success": true,
            "report": report,
        }))
        .into_response(),
        Err(e) => {
            warn!(error = %e, "config_backup_import_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
        }
    }
}

// ── POST /system/config-backup/cloud-push ───────────────────────────

/// Push the current configuration snapshot to mozaiklabs.fr cloud storage.
pub(super) async fn cloud_push(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    let snapshot = match tune_core::config_backup::export_config(&state.backend) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("export failed: {e}")})),
            )
                .into_response();
        }
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();

    if server_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no server_id configured"})),
        )
            .into_response();
    }

    let fingerprint = snapshot.fingerprint();
    let size = snapshot.size_bytes();

    let url = format!("{CLOUD_API}/{server_id}");
    let resp = state.http_client.put(&url).json(&snapshot).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let now = utc_now();
            settings.set("config_backup_last_push", &now).ok();
            settings
                .set("config_backup_last_fingerprint", &fingerprint)
                .ok();

            info!(
                server_id = %server_id,
                fingerprint = %fingerprint,
                size_bytes = size,
                "config_backup_cloud_push_done"
            );

            Json(json!({
                "success": true,
                "server_id": server_id,
                "fingerprint": fingerprint,
                "size_bytes": size,
                "pushed_at": now,
            }))
            .into_response()
        }
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            warn!(status, body = %body, "config_backup_cloud_push_error");
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({
                    "error": "cloud push failed",
                    "status": status,
                    "detail": body,
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "config_backup_cloud_push_network_error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": format!("network error: {e}"),
                })),
            )
                .into_response()
        }
    }
}

// ── POST /system/config-backup/cloud-pull ───────────────────────────

/// Pull the latest configuration snapshot from mozaiklabs.fr and restore it.
pub(super) async fn cloud_pull(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();

    if server_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no server_id configured"})),
        )
            .into_response();
    }

    let url = format!("{CLOUD_API}/{server_id}");
    let resp = state.http_client.get(&url).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let snapshot: ConfigSnapshot = match r.json().await {
                Ok(s) => s,
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({"error": format!("parse error: {e}")})),
                    )
                        .into_response();
                }
            };

            info!(
                version = %snapshot.version,
                created_at = %snapshot.created_at,
                "config_backup_cloud_pull_received"
            );

            match tune_core::config_backup::import_config(&state.backend, snapshot) {
                Ok(report) => {
                    let now = utc_now();
                    settings.set("config_backup_last_pull", &now).ok();

                    Json(json!({
                        "success": true,
                        "pulled_at": now,
                        "report": report,
                    }))
                    .into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("import failed: {e}")})),
                )
                    .into_response(),
            }
        }
        Ok(r) if r.status() == StatusCode::NOT_FOUND => Json(json!({
            "error": "no cloud backup found for this server",
            "server_id": server_id,
        }))
        .into_response(),
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            warn!(status, body = %body, "config_backup_cloud_pull_error");
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({
                    "error": "cloud pull failed",
                    "status": status,
                    "detail": body,
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "config_backup_cloud_pull_network_error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("network error: {e}")})),
            )
                .into_response()
        }
    }
}

// ── GET /system/config-backup/cloud-status ──────────────────────────

/// Show the status of cloud config backup (last push/pull dates, snapshot size).
pub(super) async fn cloud_status(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
    let last_push = settings.get("config_backup_last_push").ok().flatten();
    let last_pull = settings.get("config_backup_last_pull").ok().flatten();
    let last_fingerprint = settings
        .get("config_backup_last_fingerprint")
        .ok()
        .flatten();

    // Compute current snapshot size
    let current_size = tune_core::config_backup::export_config(&state.backend)
        .map(|s| s.size_bytes())
        .unwrap_or(0);

    Json(json!({
        "server_id": server_id,
        "last_push": last_push,
        "last_pull": last_pull,
        "last_fingerprint": last_fingerprint,
        "current_snapshot_bytes": current_size,
    }))
    .into_response()
}
