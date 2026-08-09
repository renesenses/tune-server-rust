use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use tune_core::config_backup::ConfigSnapshot;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::auth::RequireAdmin;
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

/// Wrap a snapshot in the download response shape.
fn snapshot_download(snapshot: &ConfigSnapshot) -> axum::response::Response {
    let filename = format!("tune-config-{}.json", utc_filename_stamp());
    let json_bytes = serde_json::to_vec_pretty(snapshot).unwrap_or_default();
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

// ── GET /system/config-backup/export ────────────────────────────────

/// Export the server configuration as a JSON download, **without** streaming
/// tokens.
///
/// Admin-only. This used to be reachable by any caller behind nothing but the
/// premium licence check, and the snapshot it returned carried every streaming
/// refresh token XOR'd with a key compiled into the binary (audit item 7).
///
/// Tokens now need [`export_sealed`], which takes a passphrase.
pub(super) async fn export(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    match tune_core::config_backup::export_config(&state.backend) {
        Ok(snapshot) => snapshot_download(&snapshot),
        Err(e) => {
            warn!(error = %e, "config_backup_export_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
        }
    }
}

// ── POST /system/config-backup/export ───────────────────────────────

#[derive(Deserialize)]
pub(super) struct SealedExportRequest {
    /// The token passphrase, or the recovery key.
    passphrase: String,
}

/// Export the configuration **with** streaming tokens, sealed under the
/// install's passphrase + recovery key envelope.
///
/// POST rather than GET because the passphrase belongs in a body, not in a URL
/// that lands in access logs and browser history.
pub(super) async fn export_sealed(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SealedExportRequest>,
) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }
    if body.passphrase.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "passphrase required"})),
        )
            .into_response();
    }

    match tune_core::config_backup::export_config_sealed(&state.backend, &body.passphrase) {
        Ok(snapshot) => snapshot_download(&snapshot),
        Err(e) => {
            warn!(error = %e, "config_backup_sealed_export_failed");
            (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
        }
    }
}

// ── POST /system/config-backup/import ───────────────────────────────

/// Import body. `untagged` so a bare snapshot — what every existing client
/// posts — still parses; the wrapped form carries the secret needed to unseal
/// streaming tokens.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ImportRequest {
    Wrapped {
        snapshot: Box<ConfigSnapshot>,
        /// Passphrase or recovery key. Absent = restore everything but the
        /// sealed tokens.
        #[serde(default)]
        secret: Option<String>,
    },
    Bare(Box<ConfigSnapshot>),
}

/// Import a configuration snapshot, merging with existing data.
///
/// Admin-only: this writes zones, settings and credentials. It was previously
/// reachable by any caller that passed the premium licence check.
pub(super) async fn import(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ImportRequest>,
) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    let (snapshot, secret) = match body {
        ImportRequest::Wrapped { snapshot, secret } => (*snapshot, secret),
        ImportRequest::Bare(snapshot) => (*snapshot, None),
    };

    info!(
        version = %snapshot.version,
        zones = snapshot.zones.len(),
        settings = snapshot.settings.len(),
        playlists = snapshot.playlists.len(),
        sealed_tokens = snapshot.sealed_tokens.is_some(),
        "config_backup_import_started"
    );

    match tune_core::config_backup::import_config_with_secret(
        &state.backend,
        snapshot,
        secret.as_deref(),
    ) {
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

// ── Token passphrase management ─────────────────────────────────────

/// GET /system/config-backup/passphrase — is one configured?
pub(super) async fn passphrase_status(State(state): State<AppState>) -> impl IntoResponse {
    match tune_core::config_backup::envelope_configured(&state.backend) {
        Ok(configured) => Json(json!({ "configured": configured })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct SetPassphraseRequest {
    passphrase: String,
    /// Set to true to discard an existing envelope and start over. Every
    /// snapshot sealed under the old key becomes unreadable.
    #[serde(default)]
    force_reset: bool,
}

/// POST /system/config-backup/passphrase — set up the token passphrase.
///
/// Returns the recovery key **once**. It is never stored and cannot be shown
/// again: display it to the user and tell them to keep it somewhere safe.
pub(super) async fn set_passphrase(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetPassphraseRequest>,
) -> impl IntoResponse {
    if body.passphrase.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "passphrase must be at least 8 characters"})),
        )
            .into_response();
    }

    let result = if body.force_reset {
        tune_core::config_backup::reset_envelope(&state.backend, &body.passphrase)
    } else {
        tune_core::config_backup::setup_envelope(&state.backend, &body.passphrase)
    };

    match result {
        Ok(recovery) => Json(json!({
            "success": true,
            "recovery_key": recovery.into_string(),
            "notice": "Store this recovery key now — it is shown once and cannot be recovered.",
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct ChangePassphraseRequest {
    /// The current passphrase, or the recovery key.
    current_secret: String,
    new_passphrase: String,
}

/// PUT /system/config-backup/passphrase — rotate the passphrase.
///
/// Not retroactive: snapshots already written keep opening with the old
/// passphrase. The recovery key spans both.
pub(super) async fn change_passphrase(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ChangePassphraseRequest>,
) -> impl IntoResponse {
    if body.new_passphrase.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "passphrase must be at least 8 characters"})),
        )
            .into_response();
    }

    match tune_core::config_backup::change_envelope_passphrase(
        &state.backend,
        &body.current_secret,
        &body.new_passphrase,
    ) {
        Ok(()) => Json(json!({
            "success": true,
            "notice": "Snapshots exported before this change still open with the previous \
                       passphrase; the recovery key opens both.",
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

// ── POST /system/config-backup/cloud-push ───────────────────────────

/// Push the current configuration snapshot to mozaiklabs.fr cloud storage.
#[derive(Deserialize, Default)]
pub(super) struct CloudPushRequest {
    /// Supply the token passphrase to include streaming tokens, sealed. Absent
    /// (the default) pushes a snapshot with no tokens at all.
    #[serde(default)]
    passphrase: Option<String>,
}

/// Push a snapshot to mozaiklabs.fr.
///
/// Tokens are **excluded by default**. This endpoint is why audit item 7
/// mattered: it uploads to a third-party server, and it used to send every
/// streaming refresh token XOR'd with a key present in every binary. A caller
/// that wants tokens in the cloud copy must pass the passphrase, and they
/// travel sealed — mozaiklabs.fr stores an opaque blob it cannot read.
pub(super) async fn cloud_push(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<CloudPushRequest>>,
) -> impl IntoResponse {
    if let Err(r) =
        crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
    {
        return r;
    }

    let passphrase = body.and_then(|Json(b)| b.passphrase).unwrap_or_default();
    let exported = if passphrase.is_empty() {
        tune_core::config_backup::export_config(&state.backend)
    } else {
        tune_core::config_backup::export_config_sealed(&state.backend, &passphrase)
    };

    let snapshot = match exported {
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
pub(super) async fn cloud_pull(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> impl IntoResponse {
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
