use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

fn make_app_with_state() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(json!(null));
    (status, json)
}

/// Returns (status, content_type, raw_bytes) for content-type assertions.
async fn get_raw(app: &axum::Router, path: &str) -> (StatusCode, String, bytes::Bytes) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, content_type, body)
}

/// Returns (status, content_type, raw_bytes) with a custom Accept header.
async fn get_with_accept(
    app: &axum::Router,
    path: &str,
    accept: &str,
) -> (StatusCode, String, bytes::Bytes) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(path)
                .header(header::ACCEPT, accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, content_type, body)
}

/// Asserts a content-type header contains "application/json".
fn assert_json_content_type(content_type: &str, endpoint: &str) {
    assert!(
        content_type.contains("application/json"),
        "{endpoint} returned content-type '{content_type}' instead of application/json"
    );
}

/// Asserts raw bytes are valid JSON (not HTML).
fn assert_not_html(bytes: &[u8], endpoint: &str) {
    let text = String::from_utf8_lossy(bytes);
    assert!(
        !text.trim_start().starts_with("<!"),
        "{endpoint} returned HTML instead of JSON: {}",
        &text[..text.len().min(200)]
    );
    assert!(
        !text.trim_start().starts_with("<html"),
        "{endpoint} returned HTML instead of JSON: {}",
        &text[..text.len().min(200)]
    );
    // Must parse as valid JSON
    assert!(
        serde_json::from_slice::<Value>(bytes).is_ok(),
        "{endpoint} response is not valid JSON: {}",
        &text[..text.len().min(200)]
    );
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
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
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);

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

    let (status, body) = post_json(
        &app,
        "/api/v1/tags",
        json!({"name": "Jazz", "color": "#FFD700"}),
    )
    .await;
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

    // Fresh DBs are seeded with a set of default radio stations
    // (migration seed_default_radios), so take the seeded count as baseline
    // and assert the CRUD create adds exactly one new station.
    let (_, body) = get(&app, "/api/v1/radios").await;
    let baseline = body.as_array().unwrap().len();

    let (status, _body) = post_json(
        &app,
        "/api/v1/radios",
        json!({"name": "Test Station CRUD", "url": "http://example.com/test-crud.aac"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&app, "/api/v1/radios").await;
    assert_eq!(status, StatusCode::OK);
    let radios = body.as_array().unwrap();
    assert_eq!(radios.len(), baseline + 1);
    assert!(radios.iter().any(|r| r["name"] == "Test Station CRUD"));
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

// ── Library listing tests ──────────────────────────────────────────

#[tokio::test]
async fn library_albums_empty() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/library/albums").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].is_array());
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);
    assert!(body["limit"].is_number());
    assert!(body["offset"].is_number());
}

#[tokio::test]
async fn library_artists_empty() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/library/artists").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].is_array());
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);
    assert!(body["limit"].is_number());
    assert!(body["offset"].is_number());
}

// ── Scan trigger test ─────────────────────────────────────────────

#[tokio::test]
async fn system_scan_trigger() {
    let app = make_app();

    // Trigger a scan — returns 202 Accepted
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/system/scan")
                .header("Content-Type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "scanning");

    // Scan status endpoint should report scanning or idle
    let (status, body) = get(&app, "/api/v1/system/scan/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["status"].is_string());
    let scan_state = body["status"].as_str().unwrap();
    assert!(
        scan_state == "scanning" || scan_state == "idle",
        "unexpected scan status: {scan_state}"
    );
}

// ── Error / 404 tests ─────────────────────────────────────────────

#[tokio::test]
async fn album_not_found() {
    let app = make_app();
    let (status, _) = get(&app, "/api/v1/library/albums/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nonexistent_api_route() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not found");
}

// ── Zone consistency tests ──────────────────────────────────────────

#[tokio::test]
async fn stats_zone_count_matches_db() {
    let app = make_app();

    let (_, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(body["zones"], 0);

    post_json(&app, "/api/v1/zones", json!({"name": "Salon"})).await;
    post_json(&app, "/api/v1/zones", json!({"name": "Bureau"})).await;

    let (_, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(body["zones"], 2);
}

#[tokio::test]
async fn admin_health_zone_count_matches_db() {
    let app = make_app();

    post_json(&app, "/api/v1/zones", json!({"name": "Salon"})).await;
    post_json(&app, "/api/v1/zones", json!({"name": "Bureau"})).await;
    post_json(&app, "/api/v1/zones", json!({"name": "Chambre"})).await;

    let (status, body) = get(&app, "/api/v1/system/admin/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["playback"]["zones_total"], 3,
        "admin/health must report DB zone count, not in-memory playback"
    );
}

#[tokio::test]
async fn admin_zones_returns_created_zones() {
    let app = make_app();

    post_json(
        &app,
        "/api/v1/zones",
        json!({"name": "Salon", "output_type": "dlna"}),
    )
    .await;
    post_json(&app, "/api/v1/zones", json!({"name": "Bureau"})).await;

    let (status, body) = get(&app, "/api/v1/system/admin/zones").await;
    assert_eq!(status, StatusCode::OK);
    let zones = body.as_array().unwrap();
    assert_eq!(zones.len(), 2);
    assert!(zones.iter().any(|z| z["name"] == "Salon"));
    assert!(zones.iter().any(|z| z["name"] == "Bureau"));
}

#[tokio::test]
async fn zone_delete_updates_all_counts() {
    let app = make_app();

    let (_, body) = post_json(&app, "/api/v1/zones", json!({"name": "Temp"})).await;
    let zone_id = body["id"].as_i64().unwrap();

    let (_, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(body["zones"], 1);

    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::delete(&format!("/api/v1/zones/{zone_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let (_, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(body["zones"], 0);
}

// ── Response format / parsing robustness tests ──────────────────────

#[tokio::test]
async fn stats_response_has_all_fields() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/stats").await;
    assert_eq!(status, StatusCode::OK);
    for field in [
        "artists",
        "albums",
        "tracks",
        "zones",
        "devices",
        "outputs",
        "server_version",
        "server_engine",
    ] {
        assert!(body.get(field).is_some(), "stats missing field: {field}");
    }
    assert!(body["artists"].is_number());
    assert!(body["albums"].is_number());
    assert!(body["tracks"].is_number());
    assert!(body["zones"].is_number());
}

#[tokio::test]
async fn admin_health_response_structure() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/admin/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["engine"], "rust");
    assert!(body["uptime_seconds"].is_number());
    assert!(body["database"]["tracks"].is_number());
    assert!(body["database"]["albums"].is_number());
    assert!(body["playback"]["zones_total"].is_number());
    assert!(body["playback"]["zones_playing"].is_number());
}

#[tokio::test]
async fn zone_response_has_required_fields() {
    let app = make_app();
    post_json(&app, "/api/v1/zones", json!({"name": "Test Zone"})).await;

    let (_, body) = get(&app, "/api/v1/zones").await;
    let zone = &body[0];
    for field in ["id", "name", "volume", "muted"] {
        assert!(zone.get(field).is_some(), "zone missing field: {field}");
    }
    assert!(zone["id"].is_number());
    assert!(zone["name"].is_string());
}

#[tokio::test]
async fn zone_status_response_fields() {
    let app = make_app();
    post_json(&app, "/api/v1/zones", json!({"name": "Test"})).await;

    let (status, body) = get(&app, "/api/v1/zones/1/status").await;
    assert_eq!(status, StatusCode::OK);
    for field in ["state", "volume"] {
        assert!(
            body.get(field).is_some(),
            "zone status missing field: {field}"
        );
    }
    assert!(["playing", "paused", "stopped"].contains(&body["state"].as_str().unwrap()));
}

#[tokio::test]
async fn diagnostics_returns_ok() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/diagnostics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["engine"], "rust");
    assert!(body["cpu_count"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn telemetry_snapshot_default_disabled() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/telemetry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert!(body["payload"]["version"].is_string());
    assert!(body["payload"]["os"].is_string());
    assert!(body["payload"]["tracks"].is_number());
    assert!(body["payload"]["zones"].is_number());
}

#[tokio::test]
async fn telemetry_toggle() {
    let app = make_app();

    let (status, body) =
        post_json(&app, "/api/v1/system/telemetry", json!({"enabled": true})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], true);

    let (_, body) = get(&app, "/api/v1/system/telemetry").await;
    assert_eq!(body["enabled"], true);
}

#[tokio::test]
async fn api_stats_endpoint() {
    let app = make_app();
    get(&app, "/api/v1/system/version").await;
    get(&app, "/api/v1/system/stats").await;
    get(&app, "/api/v1/system/stats").await;

    let (status, body) = get(&app, "/api/v1/system/api-stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total_requests"].as_u64().unwrap() >= 3);
    assert!(body["top_endpoints"].is_array());
    assert!(body["slowest_endpoints"].is_array());
}

#[tokio::test]
async fn changelog_has_entries() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/changelog").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["version"].is_string());
    let entries = body["entries"].as_array().unwrap();
    assert!(
        entries.len() >= 5,
        "changelog should have at least 5 versions"
    );
    // The newest entry's version is not hardcoded (it moves with each
    // release); just assert it's a present, non-empty string.
    let latest = entries[0]["version"].as_str().unwrap();
    assert!(!latest.is_empty(), "latest changelog version must be set");
}

// ── Playback e2e tests with MockOutput ──────────────────────────────

#[tokio::test]
async fn playback_zone_with_mock_output() {
    let (app, state) = make_app_with_state();

    // Create zone
    let (status, body) = post_json(
        &app,
        "/api/v1/zones",
        json!({"name": "MockZone", "output_type": "mock", "output_device_id": "mock-dev-1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let zone_id = body["id"].as_i64().unwrap();

    // Register mock output
    let mock = tune_core::outputs::mock::MockOutput::new("mock-dev-1", "Mock Device");
    {
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(mock));
    }

    // Zone should exist and be stopped
    let (status, body) = get(&app, &format!("/api/v1/zones/{zone_id}/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "stopped");
}

#[tokio::test]
async fn mock_output_registered_in_outputs() {
    let (_app, state) = make_app_with_state();

    let mock = tune_core::outputs::mock::MockOutput::new("test-output", "Test Output");
    {
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(mock));
    }

    let outputs = state.outputs.lock().await;
    assert!(outputs.get("test-output").is_some());
    let output = outputs.get("test-output").unwrap();
    let locked = output.lock().await;
    assert_eq!(locked.name(), "Test Output");
    assert_eq!(locked.output_type(), "mock");
    assert!(locked.is_available().await);
}

#[tokio::test]
async fn mock_output_status_reflects_in_admin_zones() {
    let (app, state) = make_app_with_state();

    // Create zone linked to mock output
    post_json(
        &app,
        "/api/v1/zones",
        json!({"name": "Living Room", "output_type": "mock", "output_device_id": "mock-living"}),
    )
    .await;

    let mock = tune_core::outputs::mock::MockOutput::new("mock-living", "Living Room Speaker");
    {
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(mock));
    }

    // Admin zones should include our zone
    let (status, body) = get(&app, "/api/v1/system/admin/zones").await;
    assert_eq!(status, StatusCode::OK);
    let zones = body.as_array().unwrap();
    assert!(zones.iter().any(|z| z["name"] == "Living Room"));
}

#[tokio::test]
async fn playback_manager_state_transitions() {
    let (_app, state) = make_app_with_state();

    // Create a zone in DB
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let zone_id = zone_repo
        .create("Test", Some("mock"), Some("mock-1"))
        .unwrap();

    // Initially stopped
    let zs = state.playback.get_state(zone_id).await;
    assert_eq!(zs.state, tune_core::playback::PlayState::Stopped);

    // Simulate play
    let np = tune_core::playback::NowPlaying {
        track_id: Some(1),
        title: "Track A".into(),
        artist_name: Some("Artist".into()),
        album_title: Some("Album".into()),
        cover_path: None,
        duration_ms: 256_487,
        source: "local".into(),
        source_id: None,
        stream_id: Some("stream-001".into()),
        format: None,
        sample_rate: None,
        bit_depth: None,
        genre: None,
        year: None,
    };
    state.playback.play(zone_id, np).await;
    let zs = state.playback.get_state(zone_id).await;
    assert_eq!(zs.state, tune_core::playback::PlayState::Playing);
    assert_eq!(zs.now_playing.as_ref().unwrap().title, "Track A");
    assert_eq!(zs.now_playing.as_ref().unwrap().duration_ms, 256_487);

    // Simulate advance (gapless metadata update)
    let np2 = tune_core::playback::NowPlaying {
        track_id: Some(2),
        title: "Track B".into(),
        artist_name: Some("Artist".into()),
        album_title: Some("Album".into()),
        cover_path: None,
        duration_ms: 226_000,
        source: "local".into(),
        source_id: None,
        stream_id: None,
        format: None,
        sample_rate: None,
        bit_depth: None,
        genre: None,
        year: None,
    };
    state.playback.play(zone_id, np2).await;
    let zs = state.playback.get_state(zone_id).await;
    assert_eq!(zs.state, tune_core::playback::PlayState::Playing);
    assert_eq!(zs.now_playing.as_ref().unwrap().title, "Track B");
    assert!(
        zs.now_playing.as_ref().unwrap().stream_id.is_none(),
        "gapless advance should have stream_id=None"
    );

    // Stop
    state.playback.stop(zone_id).await;
    let zs = state.playback.get_state(zone_id).await;
    assert_eq!(zs.state, tune_core::playback::PlayState::Stopped);
}

// ── API JSON response guard tests ─────────────────────────────────
// Prevents the bug class where API routes return HTML (web client
// fallback) instead of JSON.

#[tokio::test]
async fn system_endpoints_return_json_content_type() {
    let app = make_app();

    let endpoints = [
        "/api/v1/system/health",
        "/api/v1/system/stats",
        "/api/v1/system/diagnostics",
        "/api/v1/system/config",
        "/api/v1/system/database/status",
        "/api/v1/system/version",
    ];

    for endpoint in endpoints {
        let (status, content_type, body) = get_raw(&app, endpoint).await;
        assert!(status.is_success(), "{endpoint} returned status {status}");
        assert_json_content_type(&content_type, endpoint);
        assert_not_html(&body, endpoint);
    }
}

#[tokio::test]
async fn unknown_api_route_returns_json_not_html() {
    let app = make_app();

    let bogus_paths = [
        "/api/v1/nonexistent",
        "/api/v1/does/not/exist",
        "/api/v1/system/nope",
        "/api/v1/library/fake-endpoint",
    ];

    for path in bogus_paths {
        let (status, content_type, body) = get_raw(&app, path).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{path} should return 404, got {status}"
        );
        assert_json_content_type(&content_type, path);
        assert_not_html(&body, path);

        // Body must contain an error field
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("error").is_some(),
            "{path} 404 response missing 'error' field: {json}"
        );
    }
}

#[tokio::test]
async fn api_404_body_is_json_object() {
    let app = make_app();
    let (status, _, body) = get_raw(&app, "/api/v1/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let json: Value =
        serde_json::from_slice(&body).expect("404 response must be valid JSON, not HTML");
    assert!(json.is_object(), "404 body must be a JSON object");
    assert_eq!(json["error"], "not found");
}

#[tokio::test]
async fn accept_json_header_gets_json_from_api() {
    let app = make_app();

    let endpoints = [
        "/api/v1/system/health",
        "/api/v1/system/stats",
        "/api/v1/system/version",
    ];

    for endpoint in endpoints {
        let (status, content_type, body) =
            get_with_accept(&app, endpoint, "application/json").await;
        assert!(
            status.is_success(),
            "{endpoint} with Accept:json returned {status}"
        );
        assert_json_content_type(&content_type, endpoint);
        assert_not_html(&body, endpoint);
    }
}

#[tokio::test]
async fn accept_json_on_unknown_api_returns_json_404() {
    let app = make_app();

    let (status, content_type, body) =
        get_with_accept(&app, "/api/v1/nonexistent", "application/json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_json_content_type(&content_type, "/api/v1/nonexistent");
    assert_not_html(&body, "/api/v1/nonexistent");
}

#[tokio::test]
async fn api_path_never_serves_html_fallback() {
    let app = make_app();

    // Even with Accept: text/html, /api/* must NOT return HTML
    let (status, content_type, body) =
        get_with_accept(&app, "/api/v1/nonexistent", "text/html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_json_content_type(&content_type, "/api/v1/nonexistent (Accept: text/html)");
    assert_not_html(&body, "/api/v1/nonexistent (Accept: text/html)");
}

#[tokio::test]
async fn api_trailing_slash_does_not_serve_html() {
    let app = make_app();

    // Trailing-slash API paths should redirect or return JSON, never HTML
    let (status, content_type, body) = get_raw(&app, "/api/v1/nonexistent/").await;
    // The api_fallback redirects trailing slashes (301/308), or returns JSON 404
    if status == StatusCode::NOT_FOUND {
        assert_json_content_type(&content_type, "/api/v1/nonexistent/");
        assert_not_html(&body, "/api/v1/nonexistent/");
    } else {
        // Must be a redirect, not HTML
        assert!(
            status.is_redirection(),
            "/api/v1/nonexistent/ should redirect or 404, got {status}"
        );
    }
}

// ── Queue endpoint characterization tests ───────────────────────────
//
// These lock the CURRENT intentional behaviour of the unified queue
// (v0.9 rc.2: local + streaming share the `queue_items` table but keep
// independent position spaces — "one active queue type per zone"). A
// future interleaved-queue feature must update these on purpose, not by
// accident.

async fn make_zone(app: &axum::Router, name: &str) -> i64 {
    let (status, body) = post_json(app, "/api/v1/zones", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED);
    body["id"].as_i64().expect("zone id")
}

// queue_items.track_id has a FK to tracks(id) (enforced — foreign_keys=ON),
// so local queue rows require real tracks. Insert them via the repo first.
fn insert_track(state: &tune_server::state::AppState, title: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    repo.create(&tune_core::db::models::Track::new(title.into()))
        .expect("insert track")
}

#[tokio::test]
async fn queue_add_local_and_get() {
    let (app, state) = make_app_with_state();
    let t1 = insert_track(&state, "A");
    let t2 = insert_track(&state, "B");
    let zid = make_zone(&app, "Q-local").await;

    let (status, _) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "track_ids": [t1, t2] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK);
    let tracks = body["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0]["track_id"], t1);
    assert_eq!(tracks[1]["track_id"], t2);
    assert_eq!(body["length"], 2);
}

#[tokio::test]
async fn queue_add_streaming_and_get() {
    let app = make_app();
    let zid = make_zone(&app, "Q-streaming").await;

    let (status, _) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({
            "source": "qobuz",
            "source_id": "s1",
            "title": "Stream Song",
            "artist_name": "Stream Artist",
            "duration_ms": 200000
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK);
    let tracks = body["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0]["source_id"], "s1");
    assert_eq!(tracks[0]["title"], "Stream Song");
    assert_eq!(tracks[0]["source"], "qobuz");
}

#[tokio::test]
async fn queue_local_takes_priority_over_streaming() {
    // Documented behaviour: when both subsets exist, GET /queue returns the
    // LOCAL queue (streaming is the fallback only when local is empty).
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "LocalWins");
    let zid = make_zone(&app, "Q-mixed").await;

    // Add a streaming track first…
    let (s1, _) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "source": "tidal", "source_id": "t1", "title": "Streamed" }),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    // …then a local track.
    let (s2, _) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "track_ids": [tid] }),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    let (status, body) = get(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK);
    let tracks = body["tracks"].as_array().unwrap();
    // Local wins: exactly the local track, streaming item hidden.
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0]["track_id"], tid);
    assert!(tracks[0].get("source_id").is_none() || tracks[0]["source_id"].is_null());
}

#[tokio::test]
async fn queue_clear_empties_both_subsets() {
    let app = make_app();
    let zid = make_zone(&app, "Q-clear").await;

    post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "track_ids": [1, 2] }),
    )
    .await;
    post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "source": "qobuz", "source_id": "s9", "title": "X" }),
    )
    .await;

    let (status, _) = post_json(&app, &format!("/api/v1/zones/{zid}/queue/clear"), json!({})).await;
    assert!(status.is_success());

    let (status, body) = get(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["tracks"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn queue_add_empty_body_is_rejected() {
    let app = make_app();
    let zid = make_zone(&app, "Q-empty").await;

    let (status, _) = post_json(&app, &format!("/api/v1/zones/{zid}/queue/add"), json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
