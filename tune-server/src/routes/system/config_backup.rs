use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use tune_core::config_backup::ConfigSnapshot;
use tune_core::license::Feature;

use crate::auth::RequireAdmin;
use crate::state::AppState;

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
