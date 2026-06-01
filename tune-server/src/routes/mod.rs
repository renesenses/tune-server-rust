pub mod archive;
pub mod bandcamp;
pub mod bridge;
pub mod cd_rip;
pub mod connect;
pub mod dashboard;
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
pub mod setlistfm;
pub mod shazam;
pub mod siri;
pub mod smart_ai;
pub mod smart_playlists;
pub mod snapcast;
pub mod sonos;
pub mod soundcloud;
pub mod spotify_connect;
pub mod squeezebox;
pub mod streaming;
pub mod system;
pub mod tagger;
pub mod tags;
pub mod upnp;
pub mod visualizer;
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

async fn party_list_rooms(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let mgr = state.rooms.lock().await;
    axum::Json(serde_json::json!({ "rooms": mgr.list_rooms() }))
}

async fn party_create_room(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let id = body["room_id"].as_str().unwrap_or("").to_string();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, serde_json::json!({"error": "room_id required"}).to_string()).into_response();
    }
    let mut mgr = state.rooms.lock().await;
    if mgr.create_room(&id) {
        (StatusCode::CREATED, axum::Json(serde_json::json!({"room_id": id}))).into_response()
    } else {
        (StatusCode::CONFLICT, serde_json::json!({"error": "room exists"}).to_string()).into_response()
    }
}

async fn party_room_info(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mgr = state.rooms.lock().await;
    match mgr.room_info(&id) {
        Some(info) => axum::Json(info).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn party_delete_room(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> StatusCode {
    let mut mgr = state.rooms.lock().await;
    if mgr.delete_room(&id) { StatusCode::OK } else { StatusCode::NOT_FOUND }
}

async fn auto_dj_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let seed = q.get("seed_track").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let count = q.get("count").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20);
    if seed == 0 {
        return (StatusCode::BAD_REQUEST, serde_json::json!({"error": "seed_track required"}).to_string()).into_response();
    }
    let tracks = tune_core::playback::auto_dj::generate_queue(&state.db, seed, count);
    axum::Json(serde_json::json!({
        "seed_track": seed,
        "count": tracks.len(),
        "tracks": tracks,
    })).into_response()
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
    state.api_analytics.record(&path, &method, response.status().as_u16(), latency_ms);
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

async fn service_tokens_list(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> axum::Json<serde_json::Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let registry = state.services.lock().await;
    let streaming_status = registry.status_all().await;
    drop(registry);

    let tidal_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "tidal")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let qobuz_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "qobuz")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let spotify_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "spotify")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let deezer_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "deezer")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);

    let services = vec![
        serde_json::json!({
            "id": "musicbrainz", "name": "MusicBrainz", "kind": "no_auth",
            "purpose": "Années + crédits + couvertures (ID releases).",
            "pricing": "free", "pricing_note": "100 % gratuit, base de données ouverte.",
            "configured": true, "fields": [],
            "help_url": "https://musicbrainz.org/",
            "help_steps": ["Aucun token requis — MusicBrainz est gratuit et anonyme."],
        }),
        serde_json::json!({
            "id": "discogs", "name": "Discogs", "kind": "personal_token",
            "purpose": "Années + couvertures + crédits pour pressages obscurs.",
            "pricing": "free", "pricing_note": "Compte + token personnel gratuits ; API gratuite avec quota (60 req/min).",
            "configured": settings.get("discogs_token").ok().flatten().is_some(),
            "fields": [{"key": "token", "label": "Personal Access Token", "type": "password"}],
            "help_url": "https://www.discogs.com/settings/developers",
            "help_steps": ["Connecte-toi sur discogs.com.", "Va dans Settings → Developers.", "Clique 'Generate new token'.", "Colle le token ici."],
        }),
        serde_json::json!({
            "id": "lastfm", "name": "Last.fm", "kind": "api_key",
            "purpose": "Genres + scrobbling.",
            "pricing": "free", "pricing_note": "API gratuite pour usage non commercial.",
            "configured": settings.get("lastfm_api_key").ok().flatten().is_some(),
            "fields": [
                {"key": "api_key", "label": "API Key", "type": "text"},
                {"key": "api_secret", "label": "API Secret (pour scrobbling)", "type": "password"},
            ],
            "help_url": "https://www.last.fm/api/account/create",
            "help_steps": ["Va sur last.fm/api/account/create", "Renseigne un nom d'application.", "Récupère 'API key' et 'Shared secret'.", "Colle ici puis Enregistrer."],
        }),
        serde_json::json!({
            "id": "genius", "name": "Genius", "kind": "api_key",
            "purpose": "Paroles.",
            "pricing": "free", "pricing_note": "API gratuite.",
            "configured": settings.get("genius_token").ok().flatten().is_some(),
            "fields": [{"key": "token", "label": "Access Token", "type": "password"}],
            "help_url": "https://genius.com/api-clients",
            "help_steps": ["Crée un compte sur genius.com.", "Va dans API Clients.", "Crée une application et copie le token."],
        }),
        serde_json::json!({
            "id": "tidal", "name": "Tidal", "kind": "oauth",
            "purpose": "Streaming hi-res + années + couvertures.",
            "pricing": "paid", "pricing_note": "Abonnement Tidal HiFi requis (≈ 11€/mois).",
            "configured": tidal_auth, "fields": [],
            "help_url": "/streaming/tidal",
            "help_steps": ["Tidal utilise OAuth — utilise la page Streaming → Tidal pour te connecter."],
        }),
        serde_json::json!({
            "id": "qobuz", "name": "Qobuz", "kind": "login_password",
            "purpose": "Streaming hi-res + années + couvertures.",
            "pricing": "paid", "pricing_note": "Abonnement Qobuz Studio requis (≈ 13€/mois).",
            "configured": qobuz_auth, "fields": [],
            "help_url": "/streaming/qobuz",
            "help_steps": ["Qobuz utilise login/password — utilise la page Streaming → Qobuz pour te connecter."],
        }),
        serde_json::json!({
            "id": "spotify", "name": "Spotify", "kind": "oauth",
            "purpose": "Streaming + connectivité.",
            "pricing": "freemium", "pricing_note": "Compte Spotify gratuit ou Premium (≈ 11€/mois).",
            "configured": spotify_auth, "fields": [],
            "help_url": "/streaming/spotify",
            "help_steps": ["Spotify utilise OAuth — utilise la page Streaming → Spotify pour te connecter."],
        }),
        serde_json::json!({
            "id": "deezer", "name": "Deezer", "kind": "arl_token",
            "purpose": "Streaming.",
            "pricing": "freemium", "pricing_note": "Compte gratuit ou Deezer HiFi (≈ 12€/mois) pour FLAC.",
            "configured": deezer_auth,
            "fields": [{"key": "arl", "label": "ARL token (depuis cookies deezer.com)", "type": "password"}],
            "help_url": "/streaming/deezer",
            "help_steps": ["Connecte-toi sur deezer.com.", "DevTools (F12) → Application → Cookies → cherche 'arl'.", "Colle le token ARL ici."],
        }),
    ];
    axum::Json(serde_json::json!(services))
}

async fn service_token_save(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            let skey = format!("{}_{}", id, key);
            let sval = value.as_str().unwrap_or("");
            if !sval.is_empty() {
                settings.set(&skey, sval).ok();
            }
        }
    }
    axum::Json(serde_json::json!({"valid": true, "validation_message": "Token enregistré"}))
}

async fn service_token_test(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let configured = match id.as_str() {
        "lastfm" => settings.get("lastfm_api_key").ok().flatten().is_some(),
        "discogs" => settings.get("discogs_token").ok().flatten().is_some(),
        "genius" => settings.get("genius_token").ok().flatten().is_some(),
        "musicbrainz" => true,
        _ => false,
    };
    axum::Json(serde_json::json!({
        "valid": configured,
        "validation_message": if configured { "Token valide" } else { "Token manquant" },
    }))
}

async fn service_token_delete(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let keys: Vec<String> = settings
        .all()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with(&format!("{}_", id)))
        .map(|(k, _)| k)
        .collect();
    for k in &keys {
        settings.delete(k).ok();
    }
    StatusCode::NO_CONTENT
}

async fn lastfm_auth(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let token = match body["token"].as_str() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "missing token"})),
            )
                .into_response();
        }
    };
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let api_key = match settings.get("lastfm_api_key").ok().flatten() {
        Some(k) if !k.is_empty() => k,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "lastfm_api_key not configured"})),
            )
                .into_response();
        }
    };
    let api_secret = match settings.get("lastfm_api_secret").ok().flatten() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "lastfm_api_secret not configured"})),
            )
                .into_response();
        }
    };
    match tune_core::scrobble::get_session(&api_key, &api_secret, &token).await {
        Ok(session_key) => {
            settings.set("lastfm_session_key", &session_key).ok();
            axum::Json(serde_json::json!({"session_key": session_key})).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
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
    tracing::debug!(path = %path, "api_not_found");
    (StatusCode::OK, axum::Json(serde_json::json!([]))).into_response()
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
        .nest("/radio-favorites", radios::radio_favorites_router())
        .route("/radio/auto", get(auto_dj_handler))
        .route("/party/rooms", get(party_list_rooms).post(party_create_room))
        .route("/party/rooms/{id}", get(party_room_info).delete(party_delete_room))
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
        .nest("/offline", offline::router())
        .nest("/smart-ai", smart_ai::router())
        .route(
            "/services/tokens",
            get(service_tokens_list).post(service_tokens_list),
        )
        .route(
            "/services/tokens/{id}",
            axum::routing::post(service_token_save).delete(service_token_delete),
        )
        .route(
            "/services/tokens/{id}/test",
            axum::routing::post(service_token_test),
        )
        .route("/services/lastfm/auth", axum::routing::post(lastfm_auth))
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
        .map(|upnp_state| tune_core::upnp_server::standalone_router(upnp_state.clone()));

    let deezer_proxy = axum::Router::new()
        .route(
            "/deezer-proxy/{filename}",
            get(tune_core::http::deezer_proxy::handle_deezer_proxy),
        )
        .with_state(state.services.clone());

    let mut app = Router::new()
        .nest("/api/v1", api)
        .nest("/ws", ws::router())
        .nest("/api/v1/ws", ws::router())
        .nest("/ws/bridge", bridge::router())
        .with_state(state)
        .merge(tune_core::http::streamer::router(streamer_sessions))
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
    .layer(CompressionLayer::new())
    .layer(CorsLayer::permissive())
}
