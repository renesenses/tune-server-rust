use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

const IA_BASE: &str = "https://archive.org";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(ia_search))
        .route("/item/{id}", get(ia_item))
        .route("/item/{id}/files", get(ia_item_files))
        .route("/collections", get(ia_collections))
        .route("/collection/{id}", get(ia_collection))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_rows")]
    rows: u32,
    #[serde(default)]
    page: Option<u32>,
}

fn default_rows() -> u32 {
    50
}

async fn ia_search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> impl IntoResponse {
    let client = &state.http_client;

    let page = q.page.unwrap_or(1);
    let url = format!(
        "{IA_BASE}/advancedsearch.php?q={query}+AND+mediatype:audio&output=json&rows={rows}&page={page}",
        query = urlencoding::encode(&q.q),
        rows = q.rows,
    );

    match client.get(&url).send().await {
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

async fn ia_item(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let client = &state.http_client;

    let url = format!("{IA_BASE}/metadata/{id}");

    match client.get(&url).send().await {
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

async fn ia_item_files(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let client = &state.http_client;

    let url = format!("{IA_BASE}/metadata/{id}/files");

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            // Add download URLs for convenience
            let files = body["result"]
                .as_array()
                .or_else(|| body.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut f| {
                    if let Some(name) = f["name"].as_str() {
                        f["download_url"] = json!(format!("{IA_BASE}/download/{id}/{name}"));
                    }
                    f
                })
                .collect::<Vec<_>>();
            Json(json!({ "item_id": id, "files": files })).into_response()
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

async fn ia_collections(State(state): State<AppState>) -> impl IntoResponse {
    let client = &state.http_client;

    // Search for well-known audio collections
    let url = format!(
        "{IA_BASE}/advancedsearch.php?q=mediatype:collection+AND+subject:audio&output=json&rows=50"
    );

    match client.get(&url).send().await {
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
struct CollectionQuery {
    #[serde(default = "default_rows")]
    rows: u32,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    sort: Option<String>,
}

async fn ia_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<CollectionQuery>,
) -> impl IntoResponse {
    let client = &state.http_client;

    let page = q.page.unwrap_or(1);
    let sort = q.sort.unwrap_or_else(|| "downloads+desc".into());
    let url = format!(
        "{IA_BASE}/advancedsearch.php?q=collection:{id}+AND+mediatype:audio&output=json&rows={rows}&page={page}&sort[]={sort}",
        rows = q.rows,
        sort = urlencoding::encode(&sort),
    );

    match client.get(&url).send().await {
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
