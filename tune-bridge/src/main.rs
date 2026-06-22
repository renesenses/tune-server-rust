mod api_proxy;
mod protocol;
mod state;
mod stream_proxy;
mod ws_client;
mod ws_server;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use state::RelayState;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tune_bridge=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let port: u16 = std::env::var("BRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9090);

    let state = Arc::new(RelayState::new());

    // Spawn heartbeat sender
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let ping = serde_json::json!({"type": "relay.ping"}).to_string();
            for conn in heartbeat_state.servers.iter() {
                let _ = conn.ws_tx.send(ping.clone()).await;
            }
        }
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/ws/server", get(ws_server_handler))
        .route("/api/relay/{server_id}/{*path}", any(api_proxy::proxy_api))
        .route("/ws/client/{server_id}", get(ws_client::ws_client_handler))
        .route(
            "/stream/relay/{server_id}/{*stream_path}",
            get(stream_proxy::proxy_stream),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "Tune Bridge relay starting");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");

    info!("Tune Bridge listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_server_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_server::handle_server_ws(socket, state))
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    info!("shutdown signal received");
}
