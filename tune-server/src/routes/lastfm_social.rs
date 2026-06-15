use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const LASTFM_API: &str = "https://ws.audioscrobbler.com/2.0/";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/friends", get(lastfm_friends))
        .route("/friends/{user}/recent", get(friend_recent_tracks))
        .route("/friends/{user}/top-artists", get(friend_top_artists))
        .route("/neighbors", get(lastfm_neighbors))
        .route("/recommendations", get(lastfm_recommendations))
        .route("/events", get(lastfm_events))
}

fn lastfm_api_key(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get("lastfm_api_key")
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
}

fn lastfm_username(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get("lastfm_username")
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
}

async fn lastfm_call(
    client: &reqwest::Client,
    api_key: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Result<Value, String> {
    let mut query: Vec<(&str, &str)> =
        vec![("method", method), ("api_key", api_key), ("format", "json")];
    query.extend_from_slice(params);

    let resp = client
        .get(LASTFM_API)
        .query(&query)
        .send()
        .await
        .map_err(|e| format!("last.fm request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("last.fm parse error: {e}"))?;

    if !status.is_success() || body.get("error").is_some() {
        let msg = body["message"].as_str().unwrap_or("unknown error");
        return Err(format!("last.fm error: {msg}"));
    }

    Ok(body)
}

#[derive(Deserialize)]
struct UserQuery {
    user: Option<String>,
    limit: Option<String>,
}

/// Get friends list for the configured or specified Last.fm user.
async fn lastfm_friends(
    State(state): State<AppState>,
    Query(q): Query<UserQuery>,
) -> impl IntoResponse {
    let Some(api_key) = lastfm_api_key(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "lastfm_api_key not configured"})),
        )
            .into_response();
    };

    let username = q
        .user
        .or_else(|| lastfm_username(&state))
        .unwrap_or_default();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required (query param or lastfm_username setting)"})),
        )
            .into_response();
    }

    let limit = q.limit.as_deref().unwrap_or("50");
    match lastfm_call(
        &state.http_client,
        &api_key,
        "user.getFriends",
        &[("user", &username), ("limit", limit)],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct TrackQuery {
    limit: Option<String>,
}

/// Get recent tracks for a specific friend/user.
async fn friend_recent_tracks(
    State(state): State<AppState>,
    Path(user): Path<String>,
    Query(q): Query<TrackQuery>,
) -> impl IntoResponse {
    let Some(api_key) = lastfm_api_key(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "lastfm_api_key not configured"})),
        )
            .into_response();
    };

    let limit = q.limit.as_deref().unwrap_or("20");
    match lastfm_call(
        &state.http_client,
        &api_key,
        "user.getRecentTracks",
        &[("user", &user), ("limit", limit)],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Get top artists for a specific friend/user.
async fn friend_top_artists(
    State(state): State<AppState>,
    Path(user): Path<String>,
    Query(q): Query<TrackQuery>,
) -> impl IntoResponse {
    let Some(api_key) = lastfm_api_key(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "lastfm_api_key not configured"})),
        )
            .into_response();
    };

    let limit = q.limit.as_deref().unwrap_or("20");
    match lastfm_call(
        &state.http_client,
        &api_key,
        "user.getTopArtists",
        &[("user", &user), ("limit", limit)],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Get musical neighbors (users with similar taste).
async fn lastfm_neighbors(
    State(state): State<AppState>,
    Query(q): Query<UserQuery>,
) -> impl IntoResponse {
    let Some(api_key) = lastfm_api_key(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "lastfm_api_key not configured"})),
        )
            .into_response();
    };

    let username = q
        .user
        .or_else(|| lastfm_username(&state))
        .unwrap_or_default();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required"})),
        )
            .into_response();
    }

    let limit = q.limit.as_deref().unwrap_or("50");
    match lastfm_call(
        &state.http_client,
        &api_key,
        "user.getNeighbours",
        &[("user", &username), ("limit", limit)],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Get artist recommendations based on library.
async fn lastfm_recommendations(
    State(state): State<AppState>,
    Query(q): Query<UserQuery>,
) -> impl IntoResponse {
    let Some(api_key) = lastfm_api_key(&state) else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "lastfm_api_key not configured"})),
        )
            .into_response();
    };

    let username = q
        .user
        .or_else(|| lastfm_username(&state))
        .unwrap_or_default();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required"})),
        )
            .into_response();
    }

    // Last.fm deprecated user.getRecommendedArtists for non-authenticated users.
    // Fall back to getting top artists and using similar artists for recommendations.
    let limit = q.limit.as_deref().unwrap_or("10");
    match lastfm_call(
        &state.http_client,
        &api_key,
        "user.getTopArtists",
        &[("user", &username), ("limit", "5"), ("period", "3month")],
    )
    .await
    {
        Ok(top_data) => {
            let top_artists: Vec<String> = top_data
                .pointer("/topartists/artist")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // Get similar artists for the top artists
            let mut recommendations = Vec::new();
            for artist in top_artists.iter().take(3) {
                if let Ok(similar) = lastfm_call(
                    &state.http_client,
                    &api_key,
                    "artist.getSimilar",
                    &[("artist", artist), ("limit", limit)],
                )
                .await
                {
                    if let Some(artists) = similar
                        .pointer("/similarartists/artist")
                        .and_then(|a| a.as_array())
                    {
                        for a in artists {
                            recommendations.push(json!({
                                "name": a["name"],
                                "match": a["match"],
                                "url": a["url"],
                                "based_on": artist,
                            }));
                        }
                    }
                }
            }

            Json(json!({
                "recommendations": recommendations,
                "based_on_top_artists": top_artists,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Get music events near the user (stub — Last.fm events API was deprecated).
async fn lastfm_events() -> Json<Value> {
    Json(json!({
        "events": [],
        "message": "Last.fm Events API has been deprecated. Consider using Songkick or Bandsintown for event data.",
    }))
}
