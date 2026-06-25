use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(scrobbler_status))
        .route("/connect/listenbrainz", post(connect_listenbrainz))
        .route("/disconnect/{service}", post(disconnect_service))
}

/// Count how many scrobbling services are currently configured and enabled.
fn active_service_count(settings: &SettingsRepo) -> usize {
    let mut count = 0;

    // Last.fm: needs session_key + enabled
    let lastfm_session = settings
        .get("lastfm_session_key")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lastfm_enabled = settings
        .get("lastfm_scrobble_enabled")
        .ok()
        .flatten()
        .as_deref()
        != Some("false");
    if lastfm_session.is_some() && lastfm_enabled {
        count += 1;
    }

    // ListenBrainz: needs token + enabled
    let lb_token = settings
        .get("listenbrainz_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lb_enabled = settings
        .get("listenbrainz_scrobble_enabled")
        .ok()
        .flatten()
        .as_deref()
        != Some("false");
    if lb_token.is_some() && lb_enabled {
        count += 1;
    }

    count
}

/// GET /scrobbler/status
async fn scrobbler_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let is_premium = state.license.is_premium().await;

    // Last.fm
    let lastfm_session = settings
        .get("lastfm_session_key")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lastfm_username = settings
        .get("lastfm_username")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lastfm_enabled = settings
        .get("lastfm_scrobble_enabled")
        .ok()
        .flatten()
        .as_deref()
        != Some("false");

    // ListenBrainz
    let lb_token = settings
        .get("listenbrainz_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lb_username = settings
        .get("listenbrainz_username")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lb_enabled = settings
        .get("listenbrainz_scrobble_enabled")
        .ok()
        .flatten()
        .as_deref()
        != Some("false");

    let active = active_service_count(&settings);

    Json(json!({
        "tier": if is_premium { "premium" } else { "free" },
        "max_services": if is_premium { json!("unlimited") } else { json!(1) },
        "active_count": active,
        "services": {
            "lastfm": {
                "connected": lastfm_session.is_some(),
                "enabled": lastfm_enabled,
                "username": lastfm_username,
                "active": lastfm_session.is_some() && lastfm_enabled,
            },
            "listenbrainz": {
                "connected": lb_token.is_some(),
                "enabled": lb_enabled,
                "username": lb_username,
                "active": lb_token.is_some() && lb_enabled,
            },
        }
    }))
}

#[derive(Deserialize)]
struct ConnectListenBrainzBody {
    token: String,
    #[serde(default)]
    username: Option<String>,
}

/// POST /scrobbler/connect/listenbrainz
/// Save the ListenBrainz user token and optionally a username.
async fn connect_listenbrainz(
    State(state): State<AppState>,
    Json(body): Json<ConnectListenBrainzBody>,
) -> impl IntoResponse {
    if body.token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "token is required"})),
        )
            .into_response();
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let is_premium = state.license.is_premium().await;

    // Check tier gating: Free tier can only have 1 active scrobbling service.
    if !is_premium {
        // If Last.fm is already active, connecting LB would make 2 services
        let lastfm_active = settings
            .get("lastfm_session_key")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .is_some()
            && settings
                .get("lastfm_scrobble_enabled")
                .ok()
                .flatten()
                .as_deref()
                != Some("false");
        if lastfm_active {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "Free tier allows only 1 scrobbling service. Disconnect Last.fm first or upgrade to Premium.",
                    "feature": "multi_scrobbling",
                    "tier": "free",
                })),
            )
                .into_response();
        }
    }

    // Validate token against ListenBrainz API
    let client = &state.http_client;
    let validate_resp = client
        .get("https://api.listenbrainz.org/1/validate-token")
        .header("Authorization", format!("Token {}", body.token))
        .send()
        .await;

    match validate_resp {
        Ok(resp) => {
            let resp_body: Value = resp.json().await.unwrap_or(json!({}));
            let valid = resp_body["valid"].as_bool().unwrap_or(false);

            if !valid {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": "Invalid ListenBrainz token",
                        "details": resp_body["message"],
                    })),
                )
                    .into_response();
            }

            // Save token
            if let Err(e) = settings.set("listenbrainz_token", &body.token) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("Failed to save token: {e}")})),
                )
                    .into_response();
            }
            settings.set("listenbrainz_scrobble_enabled", "true").ok();

            // Save username from API response or from request body
            let username = resp_body["user_name"]
                .as_str()
                .map(String::from)
                .or(body.username);
            if let Some(ref u) = username {
                settings.set("listenbrainz_username", u).ok();
            }

            info!(
                username = ?username,
                "listenbrainz_connected"
            );

            Json(json!({
                "ok": true,
                "username": username,
                "scrobble_enabled": true,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(error = %e, "listenbrainz_validate_token_error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Failed to validate token: {e}")})),
            )
                .into_response()
        }
    }
}

/// POST /scrobbler/disconnect/{service}
async fn disconnect_service(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    match service.as_str() {
        "lastfm" => {
            settings.delete("lastfm_session_key").ok();
            settings.delete("lastfm_username").ok();
            settings.set("lastfm_scrobble_enabled", "false").ok();
            info!("scrobbler_disconnected_lastfm");
            Json(json!({"ok": true, "service": "lastfm"})).into_response()
        }
        "listenbrainz" => {
            settings.delete("listenbrainz_token").ok();
            settings.delete("listenbrainz_username").ok();
            settings.set("listenbrainz_scrobble_enabled", "false").ok();
            info!("scrobbler_disconnected_listenbrainz");
            Json(json!({"ok": true, "service": "listenbrainz"})).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Unknown scrobbling service: {service}")})),
        )
            .into_response(),
    }
}
