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

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    let web_dir = std::env::var("TUNE_WEB_DIR").unwrap_or_else(|_| "web".into());

    Router::new()
        .nest("/api/v1/system", system::router())
        .nest("/api/v1/library", library::router())
        .nest("/api/v1/library/history", history::router())
        .nest("/api/v1/zones", zones::router().merge(playback::router()))
        .nest("/api/v1/playlists", playlists::router())
        .nest("/api/v1/radios", radios::router())
        .nest("/api/v1/search", search::router())
        .nest("/api/v1/devices", devices::router())
        .nest("/api/v1/streaming", streaming::router())
        .nest("/api/v1/profiles", profiles::router())
        .nest("/api/v1/tags", tags::router())
        .nest("/api/v1/metadata", metadata::router())
        .nest("/api/v1/smart-collections", smart_playlists::router())
        .nest("/api/v1/export", export::router())
        .nest("/api/v1/network", network::router())
        .nest("/api/v1/dashboard", dashboard::router())
        .nest("/api/v1/peers", peers::router())
        .nest("/api/v1/podcasts", podcasts::router())
        .nest("/api/v1/plugins", plugins::router())
        .nest("/ws", ws::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
        .fallback_service(ServeDir::new(&web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html"))))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
}
