use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const LB_API: &str = "https://api.listenbrainz.org";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(lb_status))
        .route("/submit", post(submit_listen))
        .route("/now-playing", post(update_now_playing))
        .route("/listens", get(get_listens))
        .route("/stats", get(get_stats))
}

fn lb_token(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("listenbrainz_token")
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
}

fn lb_username(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("listenbrainz_username")
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
}


async fn lb_status(State(state): State<AppState>) -> Json<Value> {
    let token_set = lb_token(&state).is_some();
    let username = lb_username(&state);
    Json(json!({
        "configured": token_set,
        "username": username,
        "service": "listenbrainz",
    }))
}

#[derive(Deserialize)]
struct SubmitBody {
    artist: String,
    track: String,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    listened_at: Option<u64>,
}

async fn submit_listen(
    State(state): State<AppState>,
    Json(body): Json<SubmitBody>,
) -> impl IntoResponse {
    let Some(token) = lb_token(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "listenbrainz_token not configured"})),
        )
            .into_response();
    };

    let timestamp = body.listened_at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });

    let mut additional_info = json!({});
    if let Some(ref album) = body.album {
        additional_info["release_name"] = json!(album);
    }

    let payload = json!({
        "listen_type": "single",
        "payload": [{
            "listened_at": timestamp,
            "track_metadata": {
                "artist_name": body.artist,
                "track_name": body.track,
                "release_name": body.album,
                "additional_info": additional_info,
            }
        }]
    });

    match lb_submit(&state.http_client, &token, &payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn update_now_playing(
    State(state): State<AppState>,
    Json(body): Json<SubmitBody>,
) -> impl IntoResponse {
    let Some(token) = lb_token(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "listenbrainz_token not configured"})),
        )
            .into_response();
    };

    let payload = json!({
        "listen_type": "playing_now",
        "payload": [{
            "track_metadata": {
                "artist_name": body.artist,
                "track_name": body.track,
                "release_name": body.album,
            }
        }]
    });

    match lb_submit(&state.http_client, &token, &payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn lb_submit(client: &reqwest::Client, token: &str, payload: &Value) -> Result<Value, String> {
    let resp = client
        .post(format!("{LB_API}/1/submit-listens"))
        .header("Authorization", format!("Token {token}"))
        .header("Content-Type", "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("listenbrainz submit: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        warn!(status = %status, body = %body, "listenbrainz_submit_failed");
        return Err(format!("HTTP {status}: {body}"));
    }

    debug!("listenbrainz_submitted");
    serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))
}

#[derive(Deserialize)]
struct ListensQuery {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

async fn get_listens(
    State(state): State<AppState>,
    Query(q): Query<ListensQuery>,
) -> impl IntoResponse {
    let username = q
        .username
        .or_else(|| lb_username(&state))
        .unwrap_or_default();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required (query param or listenbrainz_username setting)"})),
        )
            .into_response();
    }

    let count = q.count.unwrap_or(25);
    let url = format!("{LB_API}/1/user/{username}/listens?count={count}");

    match state.http_client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("HTTP {status}: {body}")})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct StatsQuery {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    range: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

async fn get_stats(
    State(state): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> impl IntoResponse {
    let username = q
        .username
        .or_else(|| lb_username(&state))
        .unwrap_or_default();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required"})),
        )
            .into_response();
    }

    let range = q.range.unwrap_or_else(|| "week".into());
    let count = q.count.unwrap_or(25);
    let url = format!("{LB_API}/1/stats/user/{username}/artists?range={range}&count={count}");

    match state.http_client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("HTTP {status}: {body}")})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// --- Public helpers for orchestrator integration ---

/// Submit a "single" listen to ListenBrainz (fire-and-forget from orchestrator).
pub async fn submit_listen_api(
    client: &reqwest::Client,
    token: &str,
    artist: &str,
    track: &str,
    album: Option<&str>,
) -> Result<(), String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let payload = json!({
        "listen_type": "single",
        "payload": [{
            "listened_at": timestamp,
            "track_metadata": {
                "artist_name": artist,
                "track_name": track,
                "release_name": album,
            }
        }]
    });

    let resp = client
        .post(format!("{LB_API}/1/submit-listens"))
        .header("Authorization", format!("Token {token}"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("listenbrainz submit: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }

    debug!(artist, track, "listenbrainz_scrobbled");
    Ok(())
}

/// Update "playing_now" on ListenBrainz.
pub async fn update_now_playing_api(
    client: &reqwest::Client,
    token: &str,
    artist: &str,
    track: &str,
    album: Option<&str>,
) -> Result<(), String> {
    let payload = json!({
        "listen_type": "playing_now",
        "payload": [{
            "track_metadata": {
                "artist_name": artist,
                "track_name": track,
                "release_name": album,
            }
        }]
    });

    let resp = client
        .post(format!("{LB_API}/1/submit-listens"))
        .header("Authorization", format!("Token {token}"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("listenbrainz now_playing: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }

    debug!(artist, track, "listenbrainz_now_playing_updated");
    Ok(())
}
