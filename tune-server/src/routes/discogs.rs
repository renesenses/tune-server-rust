use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const DISCOGS_API: &str = "https://api.discogs.com";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(discogs_search))
        .route("/releases/{id}", get(discogs_release))
        .route("/artists/{id}", get(discogs_artist))
        .route("/artists/{id}/releases", get(discogs_artist_releases))
        .route("/labels/{id}", get(discogs_label))
        .route("/labels/{id}/releases", get(discogs_label_releases))
        .route("/masters/{id}", get(discogs_master))
}

fn discogs_token(state: &AppState) -> Option<String> {
    // Check the DB first (set via web UI), then fall back to env/toml config.
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("discogs_token")
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
        .or_else(|| state.config.discogs_token.clone().filter(|t| !t.is_empty()))
}

async fn discogs_get(
    client: &reqwest::Client,
    token: Option<&str>,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Value, (StatusCode, String)> {
    let url = format!("{DISCOGS_API}{path}");
    let mut req = client.get(&url);

    if let Some(token) = token {
        req = req.header("Authorization", format!("Discogs token={token}"));
    }

    if !params.is_empty() {
        req = req.query(params);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("discogs request: {e}")))?;

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
        Json(json!({"error": "discogs_token not configured"})),
    )
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    #[serde(rename = "type")]
    search_type: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<String>,
    #[serde(default)]
    genre: Option<String>,
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_per_page")]
    per_page: u32,
}

fn default_page() -> u32 {
    1
}
fn default_per_page() -> u32 {
    25
}

async fn discogs_search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let Some(token) = discogs_token(&state) else {
        return not_configured().into_response();
    };

    let mut params: Vec<(&str, String)> = vec![
        ("page", q.page.to_string()),
        ("per_page", q.per_page.to_string()),
    ];
    if let Some(ref query) = q.q {
        params.push(("q", query.clone()));
    }
    if let Some(ref t) = q.search_type {
        params.push(("type", t.clone()));
    }
    if let Some(ref a) = q.artist {
        params.push(("artist", a.clone()));
    }
    if let Some(ref t) = q.title {
        params.push(("title", t.clone()));
    }
    if let Some(ref y) = q.year {
        params.push(("year", y.clone()));
    }
    if let Some(ref g) = q.genre {
        params.push(("genre", g.clone()));
    }

    let param_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match discogs_get(
        &state.http_client,
        Some(&token),
        "/database/search",
        &param_refs,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn discogs_release(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let token = discogs_token(&state);
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/releases/{id}"),
        &[],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn discogs_artist(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let token = discogs_token(&state);
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/artists/{id}"),
        &[],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PaginationQuery {
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_per_page")]
    per_page: u32,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    sort_order: Option<String>,
}

async fn discogs_artist_releases(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PaginationQuery>,
) -> impl IntoResponse {
    let token = discogs_token(&state);
    let mut params: Vec<(&str, String)> = vec![
        ("page", q.page.to_string()),
        ("per_page", q.per_page.to_string()),
    ];
    if let Some(ref s) = q.sort {
        params.push(("sort", s.clone()));
    }
    if let Some(ref so) = q.sort_order {
        params.push(("sort_order", so.clone()));
    }
    let param_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/artists/{id}/releases"),
        &param_refs,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn discogs_label(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let token = discogs_token(&state);
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/labels/{id}"),
        &[],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn discogs_label_releases(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PaginationQuery>,
) -> impl IntoResponse {
    let token = discogs_token(&state);
    let params_owned = vec![
        ("page", q.page.to_string()),
        ("per_page", q.per_page.to_string()),
    ];
    let param_refs: Vec<(&str, &str)> =
        params_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/labels/{id}/releases"),
        &param_refs,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn discogs_master(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let token = discogs_token(&state);
    match discogs_get(
        &state.http_client,
        token.as_deref(),
        &format!("/masters/{id}"),
        &[],
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}
