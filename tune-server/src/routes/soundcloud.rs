use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const SC_API: &str = "https://api-v2.soundcloud.com";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(sc_search))
        .route("/tracks/{id}", get(sc_track))
        .route("/tracks/{id}/stream", get(sc_stream_url))
        .route("/users/{id}", get(sc_user))
        .route("/users/{id}/tracks", get(sc_user_tracks))
        .route("/playlists/{id}", get(sc_playlist))
        .route("/charts", get(sc_charts))
}

fn sc_client_id(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("soundcloud_client_id")
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client error: {e}"))
}

async fn sc_get(client_id: &str, path: &str, extra_params: &[(&str, &str)]) -> Result<Value, (StatusCode, String)> {
    let client = http_client().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut url = format!("{SC_API}{path}?client_id={client_id}");
    for (k, v) in extra_params {
        url.push_str(&format!("&{k}={}", urlencoding::encode(v)));
    }

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("soundcloud request: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err((StatusCode::BAD_GATEWAY, format!("HTTP {status}: {body}")));
    }

    resp.json::<Value>()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("parse: {e}")))
}

fn not_configured() -> (StatusCode, Json<Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({"error": "soundcloud_client_id not configured"})),
    )
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

async fn sc_search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(&client_id, "/search/tracks", &[("q", &q.q), ("limit", &q.limit.to_string())]).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn sc_track(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(&client_id, &format!("/tracks/{id}"), &[]).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn sc_stream_url(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    // SoundCloud stream URLs require fetching the track first, then resolving the
    // transcodings/media URL. For now, return the expected structure with the API URL.
    match sc_get(&client_id, &format!("/tracks/{id}"), &[]).await {
        Ok(track) => {
            let stream = track
                .pointer("/media/transcodings")
                .and_then(|t| t.as_array())
                .and_then(|arr| arr.first())
                .and_then(|t| t["url"].as_str())
                .map(String::from);

            Json(json!({
                "track_id": id,
                "stream_url": stream,
                "note": "stream_url must be resolved with client_id to get the actual audio URL",
            }))
            .into_response()
        }
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn sc_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(&client_id, &format!("/users/{id}"), &[]).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PaginationQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}

async fn sc_user_tracks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PaginationQuery>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(
        &client_id,
        &format!("/users/{id}/tracks"),
        &[("limit", &q.limit.to_string()), ("offset", &q.offset.to_string())],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn sc_playlist(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(&client_id, &format!("/playlists/{id}"), &[]).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

#[derive(Deserialize)]
struct ChartsQuery {
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default = "default_genre")]
    genre: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_kind() -> String {
    "top".into()
}
fn default_genre() -> String {
    "all-music".into()
}

async fn sc_charts(
    State(state): State<AppState>,
    Query(q): Query<ChartsQuery>,
) -> impl IntoResponse {
    let Some(client_id) = sc_client_id(&state) else {
        return not_configured().into_response();
    };

    match sc_get(
        &client_id,
        "/charts",
        &[
            ("kind", &q.kind),
            ("genre", &format!("soundcloud:genres:{}", q.genre)),
            ("limit", &q.limit.to_string()),
        ],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}
