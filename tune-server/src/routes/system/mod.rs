mod admin;
mod backup;
mod config;
mod convert;
mod database;
mod diagnostics;
mod enrich;
mod import;
mod playlist_hub;
mod plugins;
mod remote;
mod scan;
mod tags;
mod update;

use axum::Router;
use axum::routing::{delete, get, post};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version", get(config::version))
        .route("/health", get(config::health))
        .route("/stats", get(config::stats))
        .route(
            "/config",
            get(config::get_config).patch(config::update_config),
        )
        .route("/settings", get(config::get_settings))
        .route(
            "/settings/theme",
            axum::routing::put(config::set_theme).get(config::get_theme),
        )
        .route(
            "/settings/metadata-fields",
            get(config::get_metadata_fields).put(config::set_metadata_fields),
        )
        .route(
            "/settings/default-zone",
            get(config::get_default_zone).put(config::set_default_zone),
        )
        .route("/library/clear", post(scan::library_clear))
        .route("/scan", post(scan::trigger_scan))
        .route("/scan/status", get(scan::scan_status))
        .route("/scan/cancel", post(scan::scan_cancel))
        .route("/scan/report", get(scan::scan_report))
        .route("/restart", post(config::restart))
        .route("/database/status", get(database::database_status))
        .route("/database/optimize", post(database::database_optimize))
        .route("/database/rebuild-fts", post(database::rebuild_fts))
        .route(
            "/music-dirs",
            get(config::get_music_dirs).post(config::add_music_dir),
        )
        .route("/music-dirs/add", post(config::add_music_dir))
        .route("/music-dirs/remove", post(config::remove_music_dir))
        .route("/browse-dirs", get(config::browse_dirs))
        .route("/env", get(config::get_env))
        .route("/diagnostics", get(diagnostics::diagnostics))
        .route("/cleanup", post(enrich::cleanup))
        .route("/logs", get(diagnostics::logs))
        .route(
            "/log-level",
            get(diagnostics::get_log_level).post(diagnostics::set_log_level),
        )
        .route(
            "/backups",
            get(backup::list_backups).post(backup::create_backup),
        )
        .route("/backups/{filename}/restore", post(backup::restore_backup))
        .route("/backups/encrypt", post(backup::create_encrypted_backup))
        .route("/database/export", get(database::export_database))
        .route("/update/check", get(update::update_check))
        .route("/changelog", get(update::changelog))
        .route("/peers", get(admin::system_peers))
        .route(
            "/scan/schedule",
            get(scan::scan_schedule).post(scan::set_scan_schedule),
        )
        .route("/diagnostics/bundle", get(diagnostics::diagnostics_bundle))
        .route(
            "/diagnostics/network",
            get(diagnostics::diagnostics_network),
        )
        .route("/diagnostics/oaat", get(diagnostics::diagnostics_oaat))
        .route(
            "/bug-report/markdown",
            get(diagnostics::bug_report_markdown),
        )
        .route("/health/monitor", get(diagnostics::health_monitor))
        .route("/health/alerts", get(diagnostics::health_alerts))
        .route("/clear-cache", post(config::clear_cache))
        .route("/mode", get(config::get_mode).post(config::set_mode))
        .route("/stats/listening", get(admin::listening_stats))
        .route("/discover-servers", get(admin::discover_servers))
        .route("/config/export", get(config::export_config))
        .route("/config/import", post(config::import_config))
        // Import routes
        .route("/import/roon", post(import::import_roon))
        .route("/import/plex", post(import::import_plex))
        .route("/import/playlists", post(import::import_playlists_file))
        .route("/import/jriver", post(import::import_jriver))
        .route("/import/status/{task_id}", get(import::import_status))
        // Database engine routes
        .route(
            "/database/test-connection",
            post(database::test_db_connection),
        )
        .route("/database/migrate", post(database::migrate_database))
        // Remote/proxy mode routes
        .route(
            "/remote/config",
            get(remote::get_remote_config).post(remote::set_remote_config),
        )
        .route("/remote/status", get(remote::remote_status))
        // Admin routes
        .route("/admin/errors", get(admin::admin_errors))
        .route("/admin/connections", get(admin::admin_connections))
        .route("/admin/discovery", get(admin::admin_discovery))
        .route("/admin/health", get(admin::admin_health))
        .route("/admin/zones", get(admin::admin_zones))
        .route("/update/install", post(update::update_install))
        .route("/update/apply", post(update::update_apply))
        .route("/update/status", get(update::update_status))
        .route("/bug-report", get(diagnostics::generate_bug_report))
        .route("/audio-check", get(diagnostics::audio_check))
        .route("/audio/asio-devices", get(diagnostics::asio_devices))
        .route(
            "/telemetry",
            get(diagnostics::telemetry_snapshot).post(diagnostics::telemetry_toggle),
        )
        .route("/api-stats", get(diagnostics::api_stats))
        .route("/api-docs", get(diagnostics::api_docs))
        .route("/api-insights", get(diagnostics::api_insights))
        .route("/enrich", post(enrich::system_enrich))
        .route("/enrich-bios", post(enrich::enrich_bios))
        .route("/enrich-metadata", post(enrich::enrich_extended_metadata))
        .route("/database/import", post(database::database_import))
        .route("/plugins", get(plugins::list_system_plugins))
        .route("/supported-tags", get(tags::supported_tags))
        .route(
            "/settings/prefetch",
            get(config::get_prefetch).put(config::set_prefetch),
        )
        // Cloud audio format conversion
        .route("/convert", post(convert::convert_track))
        .route("/convert/{job_id}", get(convert::convert_status))
        .route("/convert/{job_id}/download", get(convert::convert_download))
        // Playlist Hub — cloud-based cross-service playlist manager
        .route("/playlist-hub/backup", post(playlist_hub::backup))
        .route("/playlist-hub", get(playlist_hub::list_playlists))
        .route(
            "/playlist-hub/{hub_id}",
            get(playlist_hub::get_playlist).delete(playlist_hub::delete_playlist),
        )
        .route(
            "/playlist-hub/{hub_id}/transfer",
            post(playlist_hub::transfer),
        )
        // Concert alerts — upcoming concerts for library artists
        .route("/concerts", get(concerts_handler))
        // Weekly digest — new releases from library artists
        .route("/new-releases", get(new_releases_handler))
        // AI Recommendations — discover new music based on library
        .route("/recommendations", get(recommendations_handler))
        .route(
            "/recommendations/generate",
            post(recommendations_generate_handler),
        )
}

/// GET /system/concerts — upcoming concerts for artists in the local library.
async fn concerts_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::concert_alerts::get_upcoming_concerts(&state.http_client, &instance_id)
        .await
    {
        Ok(concerts) => axum::Json(serde_json::json!({"concerts": concerts})),
        Err(e) => axum::Json(serde_json::json!({"concerts": [], "error": e})),
    }
}

/// GET /system/new-releases — new album releases from library artists (digest).
async fn new_releases_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::digest::get_new_releases(&state.http_client, &instance_id).await {
        Ok(releases) => axum::Json(serde_json::json!({"releases": releases})),
        Err(e) => axum::Json(serde_json::json!({"releases": [], "error": e})),
    }
}

/// GET /system/recommendations — get cached recommendations from cloud.
async fn recommendations_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::recommendations::get_recommendations(&state.http_client, &instance_id)
        .await
    {
        Ok(recs) => axum::Json(serde_json::json!({"recommendations": recs})),
        Err(e) => axum::Json(serde_json::json!({"recommendations": [], "error": e})),
    }
}

/// POST /system/recommendations/generate — trigger recommendation generation.
async fn recommendations_generate_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::recommendations::generate_recommendations(
        &state.backend,
        &state.http_client,
        &instance_id,
    )
    .await
    {
        Ok(recs) => axum::Json(serde_json::json!({
            "recommendations": recs,
            "count": recs.len(),
        })),
        Err(e) => axum::Json(serde_json::json!({"recommendations": [], "error": e})),
    }
}

/// Helper used by multiple sub-modules to get the configured music directories.
fn get_music_dirs_list(db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Vec<String> {
    SettingsRepo::with_backend(db.clone())
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
