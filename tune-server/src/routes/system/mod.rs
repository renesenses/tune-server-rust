mod admin;
mod backup;
mod config;
mod database;
mod diagnostics;
mod enrich;
mod import;
mod plugins;
mod remote;
mod scan;
mod tags;
mod update;

use axum::Router;
use axum::routing::{get, post};

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
