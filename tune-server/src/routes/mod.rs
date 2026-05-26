pub mod dashboard;
pub mod devices;
pub mod dj;
pub mod export;
pub mod history;
pub mod library;
pub mod metadata;
pub mod network;
pub mod party;
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

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

async fn api_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let redirect_to = if let Some(q) = uri.query() {
            format!("{trimmed}?{q}")
        } else {
            trimmed.to_string()
        };
        return axum::response::Redirect::permanent(&redirect_to).into_response();
    }
    tracing::debug!(path = %uri, "api_not_found");
    (StatusCode::OK, axum::Json(serde_json::json!([]))).into_response()
}

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    let web_dir = std::env::var("TUNE_WEB_DIR").unwrap_or_else(|_| "web".into());

    let zones_and_playback = zones::router().merge(playback::router());
    let api = Router::new()
        .nest("/system", system::router())
        .nest("/library", library::router())
        .nest("/library/history", history::router())
        .route("/zones/", get(zones::list_zones_handler).post(zones::create_zone_handler))
        .nest("/zones", zones_and_playback)
        .nest("/playlists", playlists::router())
        .nest("/radios", radios::router())
        .nest("/radio-favorites", radios::radio_favorites_router())
        .nest("/alarms", radios::alarms_router())
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
        .nest("/dj", dj::router())
        .nest("/party", party::router())
        .fallback(api_fallback);

    let app = Router::new()
        .nest("/api/v1", api)
        .nest("/ws", ws::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
        .fallback_service(ServeDir::new(&web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html"))))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive());

    app
}
