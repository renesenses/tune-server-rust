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

async fn service_tokens_list(axum::extract::State(state): axum::extract::State<crate::state::AppState>) -> axum::Json<serde_json::Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let services = vec![
        serde_json::json!({
            "id": "lastfm", "name": "Last.fm", "description": "Scrobbling et recommandations",
            "configured": settings.get("lastfm_api_key").ok().flatten().is_some(),
            "fields": [
                {"key": "api_key", "label": "API Key", "type": "text", "required": true},
                {"key": "api_secret", "label": "API Secret", "type": "password", "required": true},
                {"key": "session_key", "label": "Session Key", "type": "password", "required": false},
            ]
        }),
        serde_json::json!({
            "id": "discogs", "name": "Discogs", "description": "Enrichissement métadonnées",
            "configured": settings.get("discogs_token").ok().flatten().is_some(),
            "fields": [
                {"key": "token", "label": "Personal Access Token", "type": "password", "required": true},
            ]
        }),
        serde_json::json!({
            "id": "musicbrainz", "name": "MusicBrainz", "description": "Identification et crédits",
            "configured": true,
            "fields": [
                {"key": "user_agent", "label": "User-Agent (email)", "type": "text", "required": false},
            ]
        }),
        serde_json::json!({
            "id": "genius", "name": "Genius", "description": "Paroles",
            "configured": settings.get("genius_token").ok().flatten().is_some(),
            "fields": [
                {"key": "token", "label": "Access Token", "type": "password", "required": true},
            ]
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
    let keys: Vec<String> = settings.all().unwrap_or_default().into_iter()
        .filter(|(k, _)| k.starts_with(&format!("{}_", id)))
        .map(|(k, _)| k)
        .collect();
    for k in &keys {
        settings.delete(k).ok();
    }
    StatusCode::NO_CONTENT
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
        .nest("/system", system::router())
        .nest("/library", library::router())
        .nest("/library/history", history::router())
        .nest("/history", history::router())
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
        .route("/services/tokens", get(service_tokens_list).post(service_tokens_list))
        .route("/services/tokens/{id}", axum::routing::post(service_token_save).delete(service_token_delete))
        .route("/services/tokens/{id}/test", axum::routing::post(service_token_test))
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
