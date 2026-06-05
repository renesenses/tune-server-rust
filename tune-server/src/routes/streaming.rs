use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::streaming::traits::StreamingService;

use crate::state::AppState;

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

/// Look up a service by name. Locks the registry only long enough to clone
/// the Arc, so callers never hold the registry lock across await points.
async fn get_svc(
    state: &AppState,
    name: &str,
) -> Result<Arc<Mutex<Box<dyn StreamingService>>>, (StatusCode, String)> {
    let registry = state.services.lock().await;
    registry
        .get(name)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("unknown service: {name}")))
    // registry lock drops here
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/status", get(list_services))
        .route("/{service}/status", get(service_status))
        .route("/{service}/auth", post(service_auth))
        .route("/{service}/auth/status", get(auth_poll_status))
        .route("/{service}/logout", post(service_logout))
        .route("/{service}/disconnect", post(service_logout))
        .route("/compare", get(compare_services))
        .route("/{service}/search", get(service_search))
        .route("/{service}/albums", get(service_albums))
        .route("/{service}/albums/{album_id}", get(service_album))
        .route(
            "/{service}/albums/{album_id}/tracks",
            get(service_album_tracks),
        )
        .route("/{service}/artists/{artist_id}", get(service_artist))
        .route(
            "/{service}/artists/{artist_id}/albums",
            get(service_artist_albums),
        )
        .route(
            "/{service}/artists/{artist_id}/top-tracks",
            get(service_artist_top_tracks),
        )
        .route("/{service}/playlists", get(service_playlists))
        .route("/{service}/playlists/{playlist_id}", get(service_playlist))
        .route(
            "/{service}/playlists/{playlist_id}/tracks",
            get(service_playlist_tracks),
        )
        .route("/{service}/tracks/{track_id}", get(service_track))
        .route("/{service}/tracks/{track_id}/url", get(service_track_url))
        .route("/{service}/featured", get(service_featured))
        .route(
            "/{service}/featured/sections",
            get(service_featured_sections),
        )
        .route(
            "/{service}/featured/{section}",
            get(service_featured_section),
        )
        .route("/{service}/new-releases", get(service_new_releases))
        .route("/{service}/genres", get(service_genres))
        .route(
            "/{service}/genres/{genre_id}/albums",
            get(service_genre_albums),
        )
        .route("/{service}/favorites/{fav_type}", get(service_favorites))
        .route(
            "/{service}/favorites/{fav_type}/{item_id}",
            post(service_add_favorite).delete(service_remove_favorite),
        )
        .route("/{service}/enable", post(service_enable))
        .route("/{service}/disable", post(service_disable))
        .route("/{service}/auth/url", get(service_auth_url))
        .route("/youtube/home", get(youtube_home))
        .route("/youtube/charts", get(youtube_charts))
        .route("/youtube/moods", get(youtube_moods))
        .route("/youtube/library", get(youtube_library))
        .route("/spotify/callback", get(spotify_callback))
}

async fn list_services(State(state): State<AppState>) -> Json<Value> {
    // Timeout to avoid blocking the Settings page if a streaming service auth check hangs
    let map = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let registry = state.services.lock().await;
        let services = registry.status_all().await;
        let mut map = serde_json::Map::new();
        for svc in services {
            if let Some(name) = svc.get("name").and_then(|n| n.as_str()) {
                map.insert(name.to_string(), svc);
            }
        }
        map
    })
    .await
    .unwrap_or_default();
    Json(Value::Object(map))
}

async fn service_status(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.lock().await;
    let mut status = svc.auth_status().await;
    if !status.authenticated
        && let Ok(poll_status) = svc.authenticate(&json!({})).await
        && poll_status.authenticated
    {
        status = poll_status;
        drop(svc);
        state.save_tokens().await;
    }
    Json(json!({
        "service": service,
        "enabled": true,
        "authenticated": status.authenticated,
        "username": status.username,
        "subscription": status.subscription,
    }))
    .into_response()
}

async fn service_auth(
    State(state): State<AppState>,
    Path(service): Path<String>,
    raw_body: axum::body::Bytes,
) -> impl IntoResponse {
    let body: Option<Value> = if raw_body.is_empty() {
        None
    } else {
        serde_json::from_slice(&raw_body).ok()
    };

    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.lock().await;
    let credentials = body.unwrap_or(json!({"device_flow": true}));

    match svc.authenticate(&credentials).await {
        Ok(status) => {
            drop(svc);
            state.save_tokens().await;
            if status.authenticated {
                state.event_bus.emit(
                    "streaming.auth.success",
                    json!({
                        "service": &service,
                        "username": &status.username,
                    }),
                );
            }
            Json(json!({
                "service": service,
                "authenticated": status.authenticated,
                "username": status.username,
                "verification_url": status.verification_url,
                "user_code": status.user_code,
            }))
            .into_response()
        }
        Err(e) => {
            state.event_bus.emit(
                "streaming.auth.failed",
                json!({
                    "service": &service,
                    "error": &e,
                }),
            );
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    }
}

async fn auth_poll_status(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.lock().await;
    let poll_creds = json!({"poll": true});
    match svc.authenticate(&poll_creds).await {
        Ok(status) => {
            let authenticated = status.authenticated;
            let username = status.username.clone();
            if authenticated {
                drop(svc);
                state.save_tokens().await;
            }
            Json(json!({
                "service": service,
                "authenticated": authenticated,
                "username": username,
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "service": service,
            "authenticated": false,
            "message": e,
        }))
        .into_response(),
    }
}

async fn service_logout(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.lock().await;
    svc.logout().await.ok();
    drop(svc);
    state.save_tokens().await;
    Json(json!({ "service": service, "status": "logged_out" })).into_response()
}

async fn service_search(
    State(state): State<AppState>,
    Path(service): Path<String>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    let limit = q.limit.unwrap_or(20);

    match svc.search(&q.q, limit).await {
        Ok(results) => Json(json!(results)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_albums(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_user_albums().await {
        Ok(albums) => Json(json!(albums)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_album(
    State(state): State<AppState>,
    Path((service, album_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_album(&album_id).await {
        Ok(album) => Json(json!(album)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_album_tracks(
    State(state): State<AppState>,
    Path((service, album_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_album_tracks(&album_id).await {
        Ok(tracks) => Json(json!(tracks)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_artist(
    State(state): State<AppState>,
    Path((service, artist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_artist(&artist_id).await {
        Ok(artist) => Json(json!(artist)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_artist_albums(
    State(state): State<AppState>,
    Path((service, artist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    match svc.get_artist_albums(&artist_id).await {
        Ok(albums) => Json(json!(albums)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_artist_top_tracks(
    State(state): State<AppState>,
    Path((service, artist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    match svc.get_artist_top_tracks(&artist_id).await {
        Ok(tracks) => Json(json!(tracks)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_playlists(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_user_playlists().await {
        Ok(playlists) => Json(json!(playlists)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_playlist(
    State(state): State<AppState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_playlist(&playlist_id).await {
        Ok(playlist) => Json(json!(playlist)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_playlist_tracks(
    State(state): State<AppState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_playlist_tracks(&playlist_id).await {
        Ok(tracks) => Json(json!(tracks)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_track(
    State(state): State<AppState>,
    Path((service, track_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_track(&track_id).await {
        Ok(track) => Json(json!(track)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_track_url(
    State(state): State<AppState>,
    Path((service, track_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_track_url(&track_id, None).await {
        Ok(url) => Json(json!(url)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_featured(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_featured().await {
        Ok(items) => Json(json!(items)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_new_releases(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;

    match svc.get_new_releases().await {
        Ok(items) => Json(json!(items)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_genres(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    match svc.get_genres().await {
        Ok(genres) => Json(json!(genres)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_genre_albums(
    State(state): State<AppState>,
    Path((service, genre_id)): Path<(String, String)>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    let limit = q.limit.unwrap_or(50);
    match svc.get_genre_albums(&genre_id, limit).await {
        Ok(albums) => Json(json!(albums)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_featured_sections(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    match svc.get_featured_sections().await {
        Ok(sections) => Json(json!(sections)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_featured_section(
    State(state): State<AppState>,
    Path((service, section)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    match svc.get_featured_section(&section).await {
        Ok(albums) => Json(json!(albums)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_favorites(
    State(state): State<AppState>,
    Path((service, fav_type)): Path<(String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = svc.lock().await;
    let result = match fav_type.as_str() {
        "tracks" => svc.get_user_tracks().await.map(|t| json!({ "tracks": t })),
        "albums" => svc.get_user_albums().await.map(|a| json!({ "albums": a })),
        "artists" => svc
            .get_user_artists()
            .await
            .map(|a| json!({ "artists": a })),
        _ => Err(format!("unknown favorite type: {fav_type}")),
    };
    match result {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn service_add_favorite(
    State(state): State<AppState>,
    Path((service, fav_type, item_id)): Path<(String, String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.lock().await;
    match svc.add_favorite(&fav_type, &item_id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_remove_favorite(
    State(state): State<AppState>,
    Path((service, fav_type, item_id)): Path<(String, String, String)>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.lock().await;
    match svc.remove_favorite(&fav_type, &item_id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn service_enable(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    // Apply to in-memory service immediately (no restart needed)
    if let Ok(svc) = get_svc(&state, &service).await {
        let mut svc = svc.lock().await;
        svc.set_enabled(true);
    }

    let settings = SettingsRepo::new(state.db);
    settings
        .set(&format!("streaming_{service}_enabled"), "true")
        .ok();
    Json(json!({"service": service, "enabled": true})).into_response()
}

async fn service_disable(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    // Apply to in-memory service immediately (no restart needed)
    if let Ok(svc) = get_svc(&state, &service).await {
        let mut svc = svc.lock().await;
        svc.set_enabled(false);
    }

    let settings = SettingsRepo::new(state.db);
    settings
        .set(&format!("streaming_{service}_enabled"), "false")
        .ok();
    Json(json!({"service": service, "enabled": false})).into_response()
}

async fn service_auth_url(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.lock().await;
    match svc.authenticate(&json!({"device_flow": true})).await {
        Ok(status) => Json(json!({
            "url": status.verification_url,
            "user_code": status.user_code,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn youtube_home() -> Json<Value> {
    Json(json!({"sections": [], "message": "YouTube home not yet implemented"}))
}

async fn youtube_charts() -> Json<Value> {
    Json(json!({"charts": [], "message": "YouTube charts not yet implemented"}))
}

async fn youtube_moods() -> Json<Value> {
    Json(json!({"moods": [], "message": "YouTube moods not yet implemented"}))
}

async fn youtube_library() -> Json<Value> {
    Json(json!({"playlists": [], "albums": [], "artists": []}))
}

#[derive(Deserialize)]
struct SpotifyCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn spotify_callback(
    State(state): State<AppState>,
    Query(q): Query<SpotifyCallbackQuery>,
) -> impl IntoResponse {
    if let Some(ref error) = q.error {
        return Json(json!({"error": error})).into_response();
    }
    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing code parameter").into_response();
    };
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.lock().await;
    match svc
        .authenticate(&json!({"code": code, "state": q.state}))
        .await
    {
        Ok(status) => {
            drop(svc);
            state.save_tokens().await;
            Json(json!({
                "authenticated": status.authenticated,
                "username": status.username,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct CompareQuery {
    services: String,
    artist: Option<String>,
    album: Option<String>,
}

async fn compare_services(
    State(state): State<AppState>,
    Query(q): Query<CompareQuery>,
) -> impl IntoResponse {
    let service_names: Vec<&str> = q.services.split(',').map(|s| s.trim()).collect();
    if service_names.len() < 2 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need at least 2 services"})),
        )
            .into_response();
    }

    let query = q.artist.as_deref().or(q.album.as_deref()).unwrap_or("");
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "provide artist or album parameter"})),
        )
            .into_response();
    }

    let registry = state.services.lock().await;
    let mut results: serde_json::Map<String, Value> = serde_json::Map::new();

    for name in &service_names {
        let svc = match registry.get(name) {
            Some(s) => s,
            None => {
                results.insert(name.to_string(), json!({"error": "service not found"}));
                continue;
            }
        };
        let svc = svc.lock().await;
        match svc.search(query, 10).await {
            Ok(sr) => {
                results.insert(
                    name.to_string(),
                    json!({
                        "tracks": sr.tracks.len(),
                        "albums": sr.albums.len(),
                        "artists": sr.artists.len(),
                        "results": sr,
                    }),
                );
            }
            Err(e) => {
                results.insert(name.to_string(), json!({"error": e}));
            }
        }
    }
    drop(registry);

    Json(json!({
        "query": query,
        "services": results,
    }))
    .into_response()
}
