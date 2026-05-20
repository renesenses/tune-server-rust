use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(json!(null));
    (status, json)
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

#[tokio::test]
async fn system_version() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/version").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["engine"], "rust");
    assert!(body["version"].is_string());
}

#[tokio::test]
async fn system_health() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn system_stats() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tracks"], 0);
    assert_eq!(body["albums"], 0);
    assert_eq!(body["artists"], 0);
}

#[tokio::test]
async fn database_status() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/database/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["engine"], "sqlite");
    assert!(body["up_to_date"].as_bool().unwrap());
}

#[tokio::test]
async fn zone_crud() {
    let app = make_app();

    let (status, body) = post_json(&app, "/api/v1/zones", json!({"name": "Salon"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let zone_id = body["id"].as_i64().unwrap();
    assert!(zone_id > 0);

    let (status, body) = get(&app, "/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "Salon");

    let (status, body) = get(&app, &format!("/api/v1/zones/{zone_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Salon");
}

#[tokio::test]
async fn zone_playback_status() {
    let app = make_app();

    post_json(&app, "/api/v1/zones", json!({"name": "Test"})).await;

    let (status, body) = get(&app, "/api/v1/zones/1/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "stopped");
    assert_eq!(body["volume"], 0.5);
}

#[tokio::test]
async fn library_empty() {
    let app = make_app();

    let (status, body) = get(&app, "/api/v1/library/tracks?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
    assert!(body.as_array().unwrap().is_empty());

    let (status, body) = get(&app, "/api/v1/library/albums/count").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);

    let (status, body) = get(&app, "/api/v1/library/tracks/count").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn search_empty() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/search?q=miles").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["local"]["tracks"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn profiles_default() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/profiles").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "default");
}

#[tokio::test]
async fn tags_crud() {
    let app = make_app();

    let (status, body) = post_json(&app, "/api/v1/tags", json!({"name": "Jazz", "color": "#FFD700"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].as_i64().unwrap() > 0);

    let (status, body) = get(&app, "/api/v1/tags").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "Jazz");
}

#[tokio::test]
async fn playlist_crud() {
    let app = make_app();

    let (status, body) = post_json(&app, "/api/v1/playlists", json!({"name": "My Playlist"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let pl_id = body["id"].as_i64().unwrap();

    let (status, body) = get(&app, &format!("/api/v1/playlists/{pl_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "My Playlist");
}

#[tokio::test]
async fn streaming_services_list() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/streaming/services").await;
    assert_eq!(status, StatusCode::OK);
    let services = body.as_object().unwrap();
    assert!(services.len() >= 5);
    assert!(services.contains_key("tidal"));
    assert!(services.contains_key("qobuz"));
    assert!(services.contains_key("spotify"));
}

#[tokio::test]
async fn radio_crud() {
    let app = make_app();

    let (status, _body) = post_json(
        &app,
        "/api/v1/radios",
        json!({"name": "FIP", "url": "http://icecast.radiofrance.fr/fip-hifi.aac"}),
    ).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&app, "/api/v1/radios").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "FIP");
}

#[tokio::test]
async fn diagnostics() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/diagnostics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["engine"], "rust");
    assert!(body["cpu_count"].as_u64().unwrap() > 0);
    assert!(body["rust_engines"]["available"].as_bool().unwrap());
}

#[tokio::test]
async fn genre_tree_empty() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/library/genre-tree").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["genres"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn not_found() {
    let app = make_app();
    let (status, _) = get(&app, "/api/v1/library/tracks/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
