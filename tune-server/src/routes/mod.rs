pub mod ai;
pub mod archive;
pub mod bandcamp;
pub mod bridge;
pub mod cd_rip;
pub mod cloud;
pub mod connect;
pub mod dashboard;
pub mod deezer_proxy_handler;
pub mod devices;
pub mod discogs;
pub mod dj;
pub mod eq_pro;
pub mod export;
pub mod graphql;
pub mod history;
pub mod home;
pub mod homeassistant;
pub mod hqplayer;
pub mod hue;
pub mod i18n;
pub mod kiosk;
pub mod lastfm_social;
pub mod library;
pub mod listenbrainz;
pub mod mediasync;
pub mod metadata;
pub mod mqa;
pub mod network;
pub mod offline;
pub mod onboarding;
pub mod party;
pub mod peers;
pub mod playback;
pub mod playlist_manager;
pub mod playlists;
pub mod plugins;
pub mod podcasts;
pub mod profiles;
pub mod radios;
pub mod room_calibration;
pub mod roon_bridge;
pub mod sacd_rip;
pub mod search;
pub mod service_tokens;
pub mod setlistfm;
pub mod shazam;
pub mod siri;
pub mod smart_ai;
pub mod smart_collections;
pub mod smart_playlists;
pub mod snapcast;
pub mod sonos;
pub mod soundcloud;
pub mod spotify_connect;
pub mod squeezebox;
pub mod stream_handler;
pub mod streaming;
pub mod system;
pub mod tagger;
pub mod tags;
pub mod upnp;
pub mod upnp_media_server;
pub mod visualizer;
pub mod voice;
pub mod widget;
pub mod ws;
pub mod zone_manager;
pub mod zones;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

async fn auto_dj_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let seed = q
        .get("seed_track")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let count = q
        .get("count")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20);
    if seed == 0 {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "seed_track required"}).to_string(),
        )
            .into_response();
    }
    let tracks = tune_core::playback::auto_dj::generate_queue(&state.db, seed, count);
    axum::Json(serde_json::json!({
        "seed_track": seed,
        "count": tracks.len(),
        "tracks": tracks,
    }))
    .into_response()
}

async fn analytics_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    let latency_ms = start.elapsed().as_millis() as u32;
    state
        .api_analytics
        .record(&path, &method, response.status().as_u16(), latency_ms);
    response
}

async fn demo_library(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let demo_enabled = settings.get("demo_enabled").ok().flatten().as_deref() == Some("true");
    let demo_token = settings
        .get("demo_token")
        .ok()
        .flatten()
        .unwrap_or_default();

    if !demo_enabled {
        return (
            StatusCode::FORBIDDEN,
            serde_json::json!({"error": "demo mode disabled"}).to_string(),
        )
            .into_response();
    }

    if !demo_token.is_empty() {
        let provided = q.get("token").map(|s| s.as_str()).unwrap_or("");
        if provided != demo_token {
            return (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"error": "invalid demo token"}).to_string(),
            )
                .into_response();
        }
    }

    let albums = tune_core::db::album_repo::AlbumRepo::new(state.db.clone())
        .list(50, 0)
        .unwrap_or_default();
    let stats = tune_core::db::track_repo::TrackRepo::new(state.db.clone())
        .count()
        .unwrap_or(0);

    axum::Json(serde_json::json!({
        "demo": true,
        "read_only": true,
        "stats": { "tracks": stats },
        "albums": albums,
    }))
    .into_response()
}

async fn cache_control_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if path.starts_with("/assets/") {
        // Hashed assets (index-Bmb2F8zZ.js) — immutable, cache forever
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else if path == "/" || path.ends_with(".html") || !path.contains('.') {
        // HTML pages and SPA routes — always revalidate
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    response
}

async fn api_fallback(
    axum::extract::OriginalUri(original): axum::extract::OriginalUri,
) -> impl IntoResponse {
    let path = original.path();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let redirect_to = if let Some(q) = original.query() {
            format!("{trimmed}?{q}")
        } else {
            trimmed.to_string()
        };
        return axum::response::Redirect::permanent(&redirect_to).into_response();
    }
    tracing::warn!(path = %path, "api_not_found");
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "not found", "path": path})),
    )
        .into_response()
}

pub fn router(state: AppState) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    let web_dir = std::env::var("TUNE_WEB_DIR").unwrap_or_else(|_| "web".into());

    let zones_and_playback = zones::router().merge(playback::router());
    let api = Router::new()
        .route(
            "/playback/shuffle-all",
            axum::routing::post(playback::shuffle_all),
        )
        .nest("/system", system::router())
        .route("/demo/library", get(demo_library))
        .nest("/library", library::router())
        .nest("/library/history", history::router())
        .nest("/history", history::router())
        .route(
            "/zones/",
            get(zones::list_zones_handler).post(zones::create_zone_handler),
        )
        .nest("/zones", zones_and_playback)
        .nest("/playlists", playlists::router())
        .nest("/radios", radios::router())
        .route("/add-radio", get(radios::add_from_web))
        .nest("/radio-favorites", radios::radio_favorites_router())
        .route("/radio/auto", get(auto_dj_handler))
        .route("/voice-search", axum::routing::post(voice::voice_search))
        .route(
            "/party/rooms",
            get(party::list_rooms).post(party::create_room),
        )
        .route(
            "/party/rooms/{id}",
            get(party::room_info).delete(party::delete_room),
        )
        .nest("/alarms", radios::alarms_router())
        .nest("/search", search::router())
        .nest("/devices", devices::router())
        .nest("/streaming", streaming::router())
        .nest("/profiles", profiles::router())
        .nest("/tags", tags::router())
        .nest("/metadata", metadata::router())
        .nest("/library/smart-playlists", smart_playlists::router())
        .nest("/library/smart-collections", smart_collections::router())
        .nest("/export", export::router())
        .nest("/network", network::router())
        .nest("/dashboard", dashboard::router())
        .nest("/peers", peers::router())
        .nest("/podcasts", podcasts::router())
        .nest("/plugins", plugins::router())
        .nest("/dj", dj::router())
        .nest("/party", party::router())
        .nest("/playlist-manager", playlist_manager::router())
        .nest("/zone-manager", zone_manager::router())
        .nest("/snapcast", snapcast::router())
        .nest("/sonos", sonos::router())
        .nest("/squeezebox", squeezebox::router())
        .nest("/spotify-connect", spotify_connect::router())
        .nest("/listenbrainz", listenbrainz::router())
        .nest("/soundcloud", soundcloud::router())
        .nest("/bandcamp", bandcamp::router())
        .nest("/archive", archive::router())
        .nest("/discogs", discogs::router())
        .nest("/setlistfm", setlistfm::router())
        .nest("/homeassistant", homeassistant::router())
        .nest("/hue", hue::router())
        .nest("/tagger", tagger::router())
        .nest("/kiosk", kiosk::router())
        .nest("/widget", widget::router())
        .nest("/mediasync", mediasync::router())
        .nest("/cd-rip", cd_rip::router())
        .nest("/sacd-rip", sacd_rip::router())
        .nest("/hqplayer", hqplayer::router())
        .nest("/room-calibration", room_calibration::router())
        .nest("/visualizer", visualizer::router())
        .nest("/graphql", graphql::router())
        .nest("/eq", eq_pro::router())
        .nest("/siri", siri::router())
        .nest("/lastfm-social", lastfm_social::router())
        .nest("/mqa", mqa::router())
        .nest("/roon-bridge", roon_bridge::router())
        .nest("/connect", connect::router())
        .nest("/shazam", shazam::router())
        .nest("/home", home::router())
        .nest("/onboarding", onboarding::router())
        .nest("/i18n", i18n::router())
        .nest("/upnp", upnp::router())
        .nest("/auth", crate::auth::router())
        .nest("/cloud", cloud::router())
        .nest("/offline", offline::router())
        .nest("/smart-ai", smart_ai::router())
        .nest("/ai", ai::router())
        .route(
            "/services/tokens",
            get(service_tokens::list).post(service_tokens::list),
        )
        .route(
            "/services/tokens/{id}",
            axum::routing::post(service_tokens::save).delete(service_tokens::delete),
        )
        .route(
            "/services/tokens/{id}/test",
            axum::routing::post(service_tokens::test),
        )
        .route(
            "/services/lastfm/auth",
            axum::routing::post(service_tokens::lastfm_auth),
        )
        .fallback(api_fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            analytics_middleware,
        ));

    // UPnP MediaServer routes (ContentDirectory / ConnectionManager)
    let upnp_routes = state
        .upnp
        .as_ref()
        .map(|upnp_state| upnp_media_server::standalone_router(upnp_state.clone()));

    let deezer_proxy = axum::Router::new()
        .route(
            "/deezer-proxy/{filename}",
            get(deezer_proxy_handler::handle_deezer_proxy),
        )
        .with_state(state.services.clone());

    let mut app = Router::new()
        .nest("/api/v1", api)
        .nest("/ws", ws::router())
        .nest("/api/v1/ws", ws::router())
        .nest("/ws/bridge", bridge::router())
        .with_state(state)
        .merge(stream_handler::router(streamer_sessions))
        .merge(deezer_proxy);

    if let Some(upnp) = upnp_routes {
        app = app.nest("/upnp", upnp);
    }

    // xTune plugin — vinyl player UI
    let xtune_dir = std::env::var("TUNE_XTUNE_DIR").unwrap_or_else(|_| "xtune-web".into());
    let app = if std::path::Path::new(&xtune_dir).exists() {
        app.nest_service(
            "/xtune",
            ServeDir::new(&xtune_dir).fallback(ServeFile::new(format!("{xtune_dir}/index.html"))),
        )
    } else {
        app
    };

    let index_path = format!("{web_dir}/index.html");

    app.route(
        "/",
        get(move || async move {
            match tokio::fs::read(&index_path).await {
                Ok(html) => {
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
                    );
                    headers.insert(
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
                    );
                    (headers, html).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }),
    )
    .fallback_service(
        ServeDir::new(&web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html"))),
    )
    .layer(axum::middleware::from_fn(cache_control_middleware))
    .layer(CompressionLayer::new())
    .layer(CorsLayer::permissive())
}
