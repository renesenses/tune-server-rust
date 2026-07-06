//! YouTube playback provisioning: manage the opt-in `yt-dlp` helper binary.
//!
//! YouTube blocked Tune's unauthenticated InnerTube extraction server-side, so
//! playback goes through `yt-dlp`. These endpoints let the web UI download and
//! check the managed binary on demand ("Enable YouTube playback").

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::state::AppState;

const STATUS_KEY: &str = "ytdlp_download_status";
const VERSION_KEY: &str = "ytdlp_version";

/// GET /system/youtube/status — is YouTube playback enabled (yt-dlp present)?
pub(super) async fn youtube_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let installed = tune_core::ytdlp::binary().is_some();
    let version = settings.get(VERSION_KEY).ok().flatten();
    let status = settings.get(STATUS_KEY).ok().flatten().unwrap_or_else(|| {
        if installed {
            "ready".into()
        } else {
            "absent".into()
        }
    });
    Json(json!({
        "installed": installed,
        "version": version,
        "status": status,
    }))
}

/// POST /system/youtube/enable — download the `yt-dlp` helper in the background.
pub(super) async fn enable_youtube_playback(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    // Already installed? Report ready without re-downloading.
    if tune_core::ytdlp::binary().is_some() {
        return Json(json!({"status": "ready", "installed": true}));
    }

    settings.set(STATUS_KEY, "downloading").ok();
    let backend = state.backend.clone();
    tokio::spawn(async move {
        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend);
        match tune_core::ytdlp::download().await {
            Ok((path, tag)) => {
                settings.set("yt_dlp_path", &path.to_string_lossy()).ok();
                // Prefer the real --version; fall back to the release tag.
                let version = tune_core::ytdlp::version_of(&path).await.unwrap_or(tag);
                settings.set(VERSION_KEY, &version).ok();
                settings.set(STATUS_KEY, "ready").ok();
                tracing::info!(version = %version, "youtube_playback_enabled");
            }
            Err(e) => {
                settings.set(STATUS_KEY, &format!("failed: {e}")).ok();
                tracing::warn!(error = %e, "youtube_playback_enable_failed");
            }
        }
    });

    Json(json!({"status": "downloading", "installed": false}))
}
