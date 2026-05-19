pub mod devices;
pub mod export;
pub mod history;
pub mod library;
pub mod playback;
pub mod playlists;
pub mod profiles;
pub mod radios;
pub mod search;
pub mod streaming;
pub mod system;
pub mod tags;
pub mod ws;
pub mod zones;

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

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
        .nest("/api/v1/export", export::router())
        .nest("/ws", ws::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
}
