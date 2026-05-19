pub mod library;
pub mod playlists;
pub mod system;
pub mod zones;

use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    Router::new()
        .nest("/api/system", system::router())
        .nest("/api/library", library::router())
        .nest("/api/zones", zones::router())
        .nest("/api/playlists", playlists::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
}
