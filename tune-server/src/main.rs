mod config;
mod error;
mod routes;
mod state;

use std::net::SocketAddr;

use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::TuneConfig;
use crate::state::AppState;

#[tokio::main]
async fn main() {
    let config = TuneConfig::load();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("tune_server={}", config.log_level).parse().unwrap())
                .add_directive(format!("tune_core={}", config.log_level).parse().unwrap()),
        )
        .init();

    let state = AppState::new(&config.db_path, config.port).expect("failed to init app state");

    state.restore_tokens().await;

    if !config.music_dirs.is_empty() {
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        settings
            .set("music_dirs", &serde_json::to_string(&config.music_dirs).unwrap())
            .ok();
    }

    if config.auto_scan {
        let db = state.db.clone();
        tokio::spawn(async move {
            info!("auto_scan_starting");
            let settings = tune_core::db::settings_repo::SettingsRepo::new(db.clone());
            let music_dirs: Vec<String> = settings
                .get("music_dirs")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

            if music_dirs.is_empty() {
                info!("auto_scan_skipped_no_dirs");
                return;
            }

            let files = tune_core::scanner::walker::list_audio_files(&music_dirs);
            info!(files = files.len(), "auto_scan_files_found");

            let (_scanned, stats) =
                tune_core::scanner::walker::scan_files_parallel(&files, false, None);
            info!(
                total = stats.total_files,
                ok = stats.metadata_ok,
                failed = stats.metadata_failed,
                "auto_scan_complete"
            );
        });
    }

    info!(
        version = tune_core::version(),
        port = config.port,
        db = %config.db_path,
        web = %config.web_dir,
        "tune_server_starting"
    );

    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    info!("shutdown_signal_received");
}
