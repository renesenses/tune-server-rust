mod admin;
mod backup;
mod config;
mod config_backup;
mod convert;
mod database;
mod diagnostics;
mod enrich;
/// Périmètre de l'explorateur de dossiers (#1275).
pub(crate) mod explorateur;
// Shared enrichment quota/premium gate, reused by /library/enrich-all so the
// full-library MusicBrainz path isn't a free bypass of the same operation.
pub(crate) use enrich::gate_enrichment;
// Même partage, même raison, pour la PORTÉE par répertoire (#1660) : les deux
// routes d'enrichissement doivent valider un `path` à l'identique — refus des
// composantes `..`, appartenance à une racine musicale, refus franc plutôt que
// repli sur la bibliothèque entière. Une seconde validation écrite à côté
// finirait par diverger, et un repli silencieux enrichirait justement ce que
// l'utilisateur voulait épargner.
pub(crate) use enrich::resoudre_portee;
mod import;
mod playlist_hub;
mod plugins;
mod profile;
mod remote;
// `pub` et non `pub(crate)` : la décision « insertion ou mise à jour »
// (`verdict_ecriture`) doit être atteignable depuis un test d'intégration, qui
// est une caisse EXTERNE. Sans cette couture, le garde de #2939 aurait dû
// recopier la règle au lieu de l'appeler — un test qui réplique le code ne le
// garde pas. Les items du module restent `pub(crate)` sauf ceux exposés
// expressément.
pub mod scan;
mod tags;
pub(crate) mod update;
mod youtube;

/// Nom convivial de cette machine (#2110). Réexporté ici parce que trois
/// endroits doivent répondre la même chose à « quel serveur est-ce ? » :
/// `/system/config` (l'étiquette de l'interface), `/system/peer-info`
/// (ce que les autres serveurs lisent) et les zones unifiées multi-serveur.
pub(crate) use config::resolve_server_name;

/// Adresses complètes — schéma ET port — auxquelles ce serveur répond depuis
/// un autre appareil. Réexporté parce que le démarrage les imprime aussi
/// (#1272) : elles ne doivent exister qu'en UN endroit, sans quoi la console
/// et l'interface finiraient par annoncer deux adresses différentes.
pub(crate) use config::server_urls;

use axum::Router;
use axum::routing::{get, post};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version", get(config::version))
        .route("/health", get(config::health))
        .route("/stats", get(config::stats))
        .route("/profile", get(profile::system_profile))
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
        .route("/artist-split-preview", get(scan::artist_split_preview))
        .route("/background-tasks", get(enrich::background_tasks_status))
        // Le PASSE des passes automatiques, la ou `/background-tasks` ne dit
        // que leur present (#2080). Survit au redemarrage, borne en taille.
        .route("/task-runs", get(diagnostics::task_runs))
        .route("/restart", post(config::restart))
        .route("/stop", post(config::stop))
        .route("/database/status", get(database::database_status))
        .route("/database/optimize", post(database::database_optimize))
        .route("/database/rebuild-fts", post(database::rebuild_fts))
        .route(
            "/music-dirs",
            get(config::get_music_dirs).post(config::add_music_dir),
        )
        .route("/music-dirs/add", post(config::add_music_dir))
        .route("/music-dirs/remove", post(config::remove_music_dir))
        .route("/music-dirs/orphans", get(config::orphan_tracks))
        .route(
            "/music-dirs/purge-orphans",
            post(config::purge_orphan_tracks),
        )
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
        // Stable ou bêta : QUELLES versions `/update/check` a le droit de
        // proposer (#2266). Défaut `auto` = comportement historique.
        .route(
            "/update/channel",
            get(update::update_channel_get).put(update::update_channel_set),
        )
        .route("/changelog", get(update::changelog))
        .route(
            "/peers",
            get(admin::system_peers)
                .post(admin::add_peer)
                .delete(admin::remove_peer),
        )
        .route("/peer-info", get(admin::peer_info))
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
        .route("/bug-report/submit", post(diagnostics::submit_bug_report))
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
        .route("/youtube/status", get(youtube::youtube_status))
        .route("/youtube/enable", post(youtube::enable_youtube_playback))
        .route(
            "/license",
            get(config::get_license)
                .post(config::set_license)
                .delete(config::delete_license),
        )
        .route("/bug-report", get(diagnostics::generate_bug_report))
        .route("/audio-check", get(diagnostics::audio_check))
        .route("/audio/asio-devices", get(diagnostics::asio_devices))
        .route(
            "/audio/asio-warm-scan",
            get(diagnostics::asio_warm_scan_status),
        )
        .route(
            "/audio/asio-warm-scan/rearm",
            post(diagnostics::rearm_asio_warm_scan),
        )
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
        .route("/enrichment/status", get(enrich::enrichment_status))
        .route("/enrichment/run", post(enrich::enrichment_run))
        // La limite globale (50 Mo) coupait l'import bien avant le handler :
        // l'export d'une bibliothèque ordinaire pèse 256 Mo. Cf #2849 et
        // `database::IMPORT_DB_BODY_LIMIT`.
        .route(
            "/database/import",
            post(database::database_import).layer(axum::extract::DefaultBodyLimit::max(
                database::IMPORT_DB_BODY_LIMIT,
            )),
        )
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
        // Cloud config backup — full server config export/import/push/pull.
        // GET export omits streaming tokens; POST takes the passphrase and
        // returns them sealed (audit item 7).
        .route(
            "/config-backup/export",
            get(config_backup::export).post(config_backup::export_sealed),
        )
        .route("/config-backup/import", post(config_backup::import))
        .route(
            "/config-backup/passphrase",
            get(config_backup::passphrase_status)
                .post(config_backup::set_passphrase)
                .put(config_backup::change_passphrase),
        )
        .route("/config-backup/cloud-push", post(config_backup::cloud_push))
        .route("/config-backup/cloud-pull", post(config_backup::cloud_pull))
        .route(
            "/config-backup/cloud-status",
            get(config_backup::cloud_status),
        )
        // Weekly digest — new releases from library artists
        .route("/new-releases", get(new_releases_handler))
        // AI Recommendations — discover new music based on library
        .route("/recommendations", get(recommendations_handler))
        .route(
            "/recommendations/generate",
            post(recommendations_generate_handler),
        )
}

/// GET /system/new-releases — new album releases from library artists (digest).
async fn new_releases_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::digest::get_new_releases(&state.http_client, &instance_id).await {
        Ok(releases) => {
            axum::response::IntoResponse::into_response(axum::Json(serde_json::json!({
                "releases": releases
            })))
        }
        Err(e) => crate::routes::cloud_error::reponse(
            &e,
            &headers,
            axum::http::StatusCode::OK,
            serde_json::json!({ "releases": [] }),
        ),
    }
}

/// GET /system/recommendations — get cached recommendations from cloud.
async fn recommendations_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let instance_id = SettingsRepo::with_backend(state.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    match tune_core::cloud::recommendations::get_recommendations(&state.http_client, &instance_id)
        .await
    {
        Ok(recs) => axum::response::IntoResponse::into_response(axum::Json(serde_json::json!({
            "recommendations": recs
        }))),
        Err(e) => crate::routes::cloud_error::reponse(
            &e,
            &headers,
            axum::http::StatusCode::OK,
            serde_json::json!({ "recommendations": [] }),
        ),
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
pub(crate) fn get_music_dirs_list(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Vec<String> {
    SettingsRepo::with_backend(db.clone())
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
