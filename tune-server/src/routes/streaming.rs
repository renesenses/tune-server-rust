use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tune_core::streaming::traits::StreamingService;

use crate::state::AppState;

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
        .route("/{service}/search", get(service_search))
        .route("/{service}/albums", get(service_albums))
        .route("/{service}/albums/{album_id}", get(service_album))
        .route("/{service}/albums/{album_id}/tracks", get(service_album_tracks))
        .route("/{service}/artists/{artist_id}", get(service_artist))
        .route("/{service}/playlists", get(service_playlists))
        .route("/{service}/playlists/{playlist_id}", get(service_playlist))
        .route("/{service}/playlists/{playlist_id}/tracks", get(service_playlist_tracks))
        .route("/{service}/tracks/{track_id}", get(service_track))
        .route("/{service}/tracks/{track_id}/url", get(service_track_url))
        .route("/{service}/featured", get(service_featured))
        .route("/{service}/new-releases", get(service_new_releases))
}

async fn list_services(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let services = registry.status_all().await;
    let mut map = serde_json::Map::new();
    for svc in services {
        if let Some(name) = svc.get("name").and_then(|n| n.as_str()) {
            map.insert(name.to_string(), svc);
        }
    }
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
        && poll_status.authenticated {
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
    })).into_response()
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
            if status.authenticated {
                drop(svc);
                state.save_tokens().await;
            }
            Json(json!({
                "service": service,
                "authenticated": status.authenticated,
                "username": status.username,
                "verification_url": status.verification_url,
                "user_code": status.user_code,
            })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
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
            })).into_response()
        }
        Err(e) => Json(json!({
            "service": service,
            "authenticated": false,
            "message": e,
        })).into_response(),
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
