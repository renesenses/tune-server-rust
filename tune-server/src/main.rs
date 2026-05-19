mod routes;
mod state;

use std::net::SocketAddr;

use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("tune_server=info".parse().unwrap())
                .add_directive("tune_core=info".parse().unwrap()),
        )
        .init();

    let port: u16 = std::env::var("TUNE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8085);

    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());

    let state = AppState::new(&db_path, port).expect("failed to init app state");

    if std::env::var("TUNE_AUTO_SCAN").unwrap_or_default() == "true" {
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

            let (scanned, stats) = tune_core::scanner::walker::scan_files_parallel(&files, false, None);
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
        port,
        db = %db_path,
        "tune_server_starting"
    );

    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
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
