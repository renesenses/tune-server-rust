use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

const BC_SEARCH_API: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const BC_DISCOVER_API: &str = "https://bandcamp.com/api/discover/3/get_web";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(bc_search))
        .route("/discover", get(bc_discover))
        .route("/album/{id}", get(bc_album))
        .route("/artist/{id}", get(bc_artist))
        .route("/tags", get(bc_tags))
        .route("/tag/{tag}", get(bc_tag_releases))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn bc_search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> impl IntoResponse {
    let client = &state.http_client;

    let resp = client.get(BC_SEARCH_API).query(&[("q", &q.q)]).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: Value = r.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
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
struct DiscoverQuery {
    #[serde(default = "default_tag")]
    tag: String,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
}

fn default_tag() -> String {
    "electronic".into()
}
fn default_sort() -> String {
    "top".into()
}

async fn bc_discover(State(state): State<AppState>, Query(q): Query<DiscoverQuery>) -> impl IntoResponse {
    let client = &state.http_client;

    let payload = json!({
        "tag_norm_names": [q.tag],
        "sort": q.sort,
        "page": q.page,
    });

    let resp = client.post(BC_DISCOVER_API).json(&payload).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: Value = r.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
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

async fn bc_album(Path(id): Path<String>) -> Json<Value> {
    Json(json!({
        "id": id,
        "type": "album",
        "message": "Bandcamp has no public album API. Use /search or /discover to find releases.",
        "tracks": [],
    }))
}

async fn bc_artist(Path(id): Path<String>) -> Json<Value> {
    Json(json!({
        "id": id,
        "type": "artist",
        "message": "Bandcamp has no public artist API. Use /search to find artists.",
        "albums": [],
    }))
}

async fn bc_tags() -> Json<Value> {
    // Bandcamp's main genre tags (no public API, but these are the well-known ones)
    Json(json!({
        "tags": [
            "electronic", "ambient", "experimental", "hip-hop-rap", "rock", "metal",
            "punk", "pop", "folk", "jazz", "classical", "soul", "r-b-soul", "world",
            "soundtrack", "latin", "country", "blues", "reggae", "audiobooks",
            "podcasts", "kids", "comedy", "spoken-word", "indie",
        ]
    }))
}

#[derive(Deserialize)]
struct TagQuery {
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
}

async fn bc_tag_releases(State(state): State<AppState>, Path(tag): Path<String>, Query(q): Query<TagQuery>) -> impl IntoResponse {
    let client = &state.http_client;

    let payload = json!({
        "tag_norm_names": [tag],
        "sort": q.sort,
        "page": q.page,
    });

    let resp = client.post(BC_DISCOVER_API).json(&payload).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: Value = r.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
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
