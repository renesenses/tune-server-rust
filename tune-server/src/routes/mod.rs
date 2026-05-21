pub mod dashboard;
pub mod devices;
pub mod export;
pub mod history;
pub mod library;
pub mod metadata;
pub mod network;
pub mod playback;
pub mod playlists;
pub mod plugins;
pub mod podcasts;
pub mod peers;
pub mod profiles;
pub mod radios;
pub mod search;
pub mod smart_playlists;
pub mod streaming;
pub mod system;
pub mod tags;
pub mod ws;
pub mod zones;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

async fn normalize_trailing_slash(req: Request, next: Next) -> impl IntoResponse {
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path();
    if path.len() > 1 && path.ends_with('/') {
        let new_path = path.trim_end_matches('/');
        let new_uri = if let Some(q) = parts.uri.query() {
            format!("{new_path}?{q}")
        } else {
            new_path.to_string()
        };
        if let Ok(uri) = new_uri.parse() {
            parts.uri = uri;
        }
    }
    next.run(Request::from_parts(parts, body)).await
}

use crate::state::AppState;

async fn api_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    tracing::debug!(path = %uri, "api_not_found");
    (
        StatusCode::OK,
        axum::Json(serde_json::json!([])),
    )
}

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    let web_dir = std::env::var("TUNE_WEB_DIR").unwrap_or_else(|_| "web".into());

    let zones_router = zones::router().merge(playback::router());
    let api = Router::new()
        .nest("/system", system::router())
        .nest("/library", library::router())
        .nest("/library/history", history::router())
        .nest("/zones", zones_router.clone())
        .nest("/zones/", zones_router)
        .nest("/playlists", playlists::router())
        .nest("/radios", radios::router())
        .nest("/search", search::router())
        .nest("/devices", devices::router())
        .nest("/streaming", streaming::router())
        .nest("/profiles", profiles::router())
        .nest("/tags", tags::router())
        .nest("/metadata", metadata::router())
        .nest("/smart-collections", smart_playlists::router())
        .nest("/export", export::router())
        .nest("/network", network::router())
        .nest("/dashboard", dashboard::router())
        .nest("/peers", peers::router())
        .nest("/podcasts", podcasts::router())
        .nest("/plugins", plugins::router())
        .fallback(api_fallback);

    Router::new()
        .nest("/api/v1", api)
        .nest("/ws", ws::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
        .fallback_service(ServeDir::new(&web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html"))))
        .layer(middleware::from_fn(normalize_trailing_slash))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
}
