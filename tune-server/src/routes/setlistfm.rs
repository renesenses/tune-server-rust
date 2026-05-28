use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const SETLISTFM_API: &str = "https://api.setlist.fm/rest/1.0";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_setlists))
        .route("/artist/{mbid}/setlists", get(artist_setlists))
        .route("/setlist/{id}", get(get_setlist))
        .route("/venue/{id}/setlists", get(venue_setlists))
}

fn setlistfm_api_key(state: &AppState) -> Option<String> {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("setlistfm_api_key")
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client error: {e}"))
}

async fn setlistfm_get(
    api_key: &str,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Value, (StatusCode, String)> {
    let client = http_client().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let url = format!("{SETLISTFM_API}{path}");

    let mut req = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("Accept", "application/json");

    if !params.is_empty() {
        req = req.query(params);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("setlist.fm request: {e}")))?;

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
        Json(json!({"error": "setlistfm_api_key not configured"})),
    )
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    artist_name: Option<String>,
    #[serde(default)]
    city_name: Option<String>,
    #[serde(default)]
    venue_name: Option<String>,
    #[serde(default)]
    year: Option<String>,
    #[serde(default = "default_page")]
    p: u32,
}

fn default_page() -> u32 {
    1
}

async fn search_setlists(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let Some(api_key) = setlistfm_api_key(&state) else {
        return not_configured().into_response();
    };

    let mut params: Vec<(&str, String)> = vec![("p", q.p.to_string())];
    if let Some(ref name) = q.artist_name {
        params.push(("artistName", name.clone()));
    }
    if let Some(ref city) = q.city_name {
        params.push(("cityName", city.clone()));
    }
    if let Some(ref venue) = q.venue_name {
        params.push(("venueName", venue.clone()));
    }
    if let Some(ref year) = q.year {
        params.push(("year", year.clone()));
    }

    let param_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match setlistfm_get(&api_key, "/search/setlists", &param_refs).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default = "default_page")]
    p: u32,
}

async fn artist_setlists(
    State(state): State<AppState>,
    Path(mbid): Path<String>,
    Query(q): Query<PageQuery>,
) -> impl IntoResponse {
    let Some(api_key) = setlistfm_api_key(&state) else {
        return not_configured().into_response();
    };

    let params = [("p", q.p.to_string())];
    let param_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match setlistfm_get(&api_key, &format!("/artist/{mbid}/setlists"), &param_refs).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn get_setlist(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(api_key) = setlistfm_api_key(&state) else {
        return not_configured().into_response();
    };

    match setlistfm_get(&api_key, &format!("/setlist/{id}"), &[]).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}

async fn venue_setlists(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> impl IntoResponse {
    let Some(api_key) = setlistfm_api_key(&state) else {
        return not_configured().into_response();
    };

    let params = [("p", q.p.to_string())];
    let param_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match setlistfm_get(&api_key, &format!("/venue/{id}/setlists"), &param_refs).await {
        Ok(data) => Json(data).into_response(),
        Err((status, msg)) => (status, Json(json!({"error": msg}))).into_response(),
    }
}
