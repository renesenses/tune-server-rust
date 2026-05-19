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

    axum::serve(listener, app).await.unwrap();
}
