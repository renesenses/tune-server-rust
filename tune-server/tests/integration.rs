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

async fn patch_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::patch(path)
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
async fn smart_routes_remain_mounted_after_extraction() {
    let app = make_app();

    for path in [
        "/api/v1/library/smart-playlists",
        "/api/v1/library/smart-collections",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(body.is_array(), "{path}: {body}");
    }

    let (status, body) = post_json(
        &app,
        "/api/v1/smart-ai/generate",
        json!({"prompt": "jazz", "limit": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tracks"], json!([]));
    assert_eq!(body["total"], 0);
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

/// `/system/changelog` répond, et ce qu'il rend est bien formé.
///
/// Ce test a bloqué TOUTES les fusions du dépôt pendant une panne GitHub du
/// 2026-08-17 — y compris une PR qui ne touchait pas au changelog. La raison :
/// il exigeait au moins 5 versions d'un point d'entrée qui va les chercher sur
/// le réseau (`fetch_github_changelog` : proxy `mozaiklabs.fr` d'abord, puis
/// `api.github.com`). Les deux sources sont tombées ensemble — le proxy parce
/// que son amont EST GitHub — et le test a échoué sur `release/v0.9` comme sur
/// chaque branche.
///
/// Un test d'intégration ne doit pas transformer l'indisponibilité d'un tiers
/// en échec de compilation. Ce qui est vérifié ici reste donc :
///
/// - la route répond 200 et porte une `version` ;
/// - `entries` est un tableau ;
/// - **quand des données arrivent**, leur forme est vérifiée entièrement — au
///   moins 5 versions, la plus récente non vide.
///
/// La seule chose relâchée est l'exigence que le réseau réponde. Un changelog
/// vide n'est plus un échec ; un changelog mal formé en reste un.
#[tokio::test]
async fn changelog_has_entries() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/system/changelog").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["version"].is_string());
    let entries = body["entries"]
        .as_array()
        .expect("entries doit toujours être un tableau, même vide");

    if entries.is_empty() {
        // Source injoignable : c'est un fait sur le réseau, pas un défaut du
        // serveur. On le dit dans la sortie du test plutôt que de faire
        // échouer la CI de tout le dépôt.
        eprintln!("changelog vide — source distante injoignable, contrat de forme non vérifiable");
        return;
    }

    assert!(
        entries.len() >= 5,
        "changelog reçu mais tronqué : {} version(s), 5 attendues",
        entries.len()
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
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
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
        // Une piste de bibliotheque porte ses identifiants (#2345).
        album_id: Some(10),
        artist_id: Some(20),
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
        // Une piste de bibliotheque porte ses identifiants (#2345).
        album_id: Some(10),
        artist_id: Some(20),
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
async fn queue_returns_combined_in_insertion_order() {
    // Documented behaviour (v0.9 unified queue): local and streaming rows live
    // in ONE `queue_items` table sharing a single position space, so GET /queue
    // returns them in INSERTION order (ORDER BY position) — the exact order the
    // poller/orchestrator advance through. Both subsets are always returned
    // together (the old either/or logic hid streaming rows when a local queue
    // was present, so an added Qobuz track was invisible and never played —
    // Progman). Here the streaming track is added first, so it comes first.
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "LocalSecond");
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
    // Both are returned, in the order they were added: streaming (position 0)
    // then local (position 1).
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0]["source_id"], "t1");
    assert_eq!(tracks[1]["track_id"], tid);
    assert!(tracks[1].get("source_id").is_none() || tracks[1]["source_id"].is_null());
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

// ── Orphan zone guard (Yacine, 24/07) ───────────────────────────────
//
// A zone row without output_device_id (leftover from manual creation or
// old delete/re-create cycles) can never produce sound: send_to_output is
// skipped and play() used to "succeed" with output_sent=false, so the
// client showed the track playing while nothing came out. play/next/
// previous must now return a clean 409 and GET /zones must report the
// zone offline so clients grey it out. The zone row itself is preserved
// (no automatic destruction of user data).

#[tokio::test]
async fn orphan_zone_play_returns_409() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Orphan Track");
    // make_zone POSTs only a name: no output_device_id → orphan zone.
    let zid = make_zone(&app, "Orphan Zone").await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "track_id": tid }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "play must 409, got {body}");
    assert_eq!(body["error"], "zone_no_output_device");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("Orphan Zone"),
        "message should name the zone: {body}"
    );

    // The zone row must still exist (no automatic deletion).
    let (status, zone) = get(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(zone["name"], "Orphan Zone");
    // …and be reported offline so clients grey it out.
    assert_eq!(zone["online"], false, "orphan zone must be offline: {zone}");
}

#[tokio::test]
async fn orphan_zone_next_and_previous_return_409() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Orphan Next");
    let zid = make_zone(&app, "Orphan Nav").await;
    // Give the zone a queue so next/previous have something to skip to.
    post_json(
        &app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "track_ids": [tid] }),
    )
    .await;

    let (status, body) = post_json(&app, &format!("/api/v1/zones/{zid}/next"), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "next must 409, got {body}");
    assert_eq!(body["error"], "zone_no_output_device");

    let (status, body) = post_json(&app, &format!("/api/v1/zones/{zid}/previous"), json!({})).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "previous must 409, got {body}"
    );
    assert_eq!(body["error"], "zone_no_output_device");
}

#[tokio::test]
async fn orphan_zone_listed_offline_in_zones() {
    let app = make_app();
    let zid = make_zone(&app, "Orphan Listed").await;

    let (status, body) = get(&app, "/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK);
    let zones = body.as_array().expect("zones array");
    let zone = zones
        .iter()
        .find(|z| z["id"] == zid)
        .expect("orphan zone present in listing");
    assert_eq!(
        zone["online"], false,
        "orphan zone must be listed offline: {zone}"
    );
}

#[tokio::test]
async fn browser_zone_without_device_is_not_rejected_as_orphan() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Browser Track");
    let (status, body) = post_json(
        &app,
        "/api/v1/zones",
        json!({ "name": "Browser Zone", "output_type": "browser" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create browser zone: {body}");
    let zid = body["id"].as_i64().expect("zone id");

    // Browser zones legitimately have no output device (the web client pulls
    // stream_url itself): the orphan guard must NOT fire. The play may fail
    // for other reasons (test track has no real file) but never with 409
    // zone_no_output_device.
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "track_id": tid }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::CONFLICT,
        "browser zone must not be rejected as orphan: {body}"
    );

    // And it stays online in the listing.
    let (status, zone) = get(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(zone["online"], true, "browser zone must be online: {zone}");
}

// ───────────────────────── Lyrics endpoint (mode « Grand écran ») ─────────
//
// Contract with the web client:
//   200 {"synced": bool, "source": "lrc"|"tag"|"lrclib",
//        "lines": [{"t_ms": u64|null, "text": "..."}]}
//   404 {"error": "no_lyrics"}
// Cascade: sidecar .lrc → embedded tag → LRCLIB (opt-in). These tests cover
// the local sources and the clean-404 paths (no network involved: the
// lyrics_lrclib_enabled setting stays unset → LRCLIB is skipped).

fn insert_track_with_file(state: &tune_server::state::AppState, title: &str, path: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(title.into());
    t.file_path = Some(path.into());
    repo.create(&t).expect("insert track")
}

#[tokio::test]
async fn lyrics_unknown_track_is_404_no_lyrics() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/library/tracks/424242/lyrics").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no_lyrics");
}

#[tokio::test]
async fn lyrics_track_without_any_source_is_404_no_lyrics() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Muette");
    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no_lyrics");
}

#[tokio::test]
async fn lyrics_sidecar_lrc_is_synced() {
    let dir = std::env::temp_dir().join(format!("tune_lyrics_it_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let audio = dir.join("Ma Chanson.flac");
    // Multi-timestamps on one line + metadata tags to ignore.
    std::fs::write(
        dir.join("Ma Chanson.lrc"),
        "[ar:Artiste]\n[ti:Ma Chanson]\n[00:12.00][01:15.00]Refrain\n[00:30.500] Couplet\n",
    )
    .unwrap();

    let (app, state) = make_app_with_state();
    let tid = insert_track_with_file(&state, "Ma Chanson", audio.to_str().unwrap());

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["synced"], true);
    assert_eq!(body["source"], "lrc");
    let lines = body["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 3);
    // Sorted by t_ms; the multi-timestamp line appears twice.
    assert_eq!(lines[0]["t_ms"], 12_000);
    assert_eq!(lines[0]["text"], "Refrain");
    assert_eq!(lines[1]["t_ms"], 30_500);
    assert_eq!(lines[1]["text"], "Couplet");
    assert_eq!(lines[2]["t_ms"], 75_000);
    assert_eq!(lines[2]["text"], "Refrain");
}

#[tokio::test]
async fn lyrics_embedded_tag_plain_is_unsynced() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Taggée");
    // The scanner persists embedded USLT/LYRICS content under this key.
    let meta =
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone());
    meta.set(tid, "lyrics", "Première ligne\n\nDeuxième ligne")
        .unwrap();

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["synced"], false);
    assert_eq!(body["source"], "tag");
    let lines = body["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["t_ms"], Value::Null);
    assert_eq!(lines[0]["text"], "Première ligne");
    assert_eq!(lines[1]["text"], "Deuxième ligne");
}

#[tokio::test]
async fn lyrics_embedded_tag_with_lrc_timestamps_is_synced() {
    let (app, state) = make_app_with_state();
    let tid = insert_track(&state, "Taggée LRC");
    let meta =
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone());
    meta.set(tid, "lyrics", "[00:01.00] Un\n[00:02.00] Deux")
        .unwrap();

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["synced"], true);
    assert_eq!(body["source"], "tag");
    let lines = body["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["t_ms"], 1_000);
    assert_eq!(lines[1]["t_ms"], 2_000);
}

// ── AutoPlay : le reglage persiste, l'API le niait (Sandro, 0.9.70) ────────
//
// `autoplay_enabled` est VOLONTAIREMENT absent de la requete SQL de ZoneRepo
// (migration v36 pouvant echouer en silence sous Windows), donc `row_to_zone`
// le met a `false` sans exception. La serialisation de la zone propageait ce
// faux jusqu'au client : le bouton retombait a chaque resynchronisation alors
// que le poller, lui, lisait la bonne valeur en fin de file.

#[tokio::test]
async fn autoplay_active_est_rapporte_par_la_liste_des_zones() {
    let app = make_app();
    let zid = make_zone(&app, "AutoPlay Liste").await;

    let (status, _) = patch_json(
        &app,
        &format!("/api/v1/zones/{zid}"),
        json!({ "autoplay_enabled": true }),
    )
    .await;
    assert!(status.is_success(), "activation refusee : {status}");

    let (status, body) = get(&app, "/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK);
    let zone = body
        .as_array()
        .expect("zones array")
        .iter()
        .find(|z| z["id"] == zid)
        .expect("zone presente");
    assert_eq!(
        zone["autoplay_enabled"], true,
        "la liste doit rapporter le reglage persiste : {zone}"
    );
}

#[tokio::test]
async fn autoplay_active_est_rapporte_par_le_detail_de_la_zone() {
    let app = make_app();
    let zid = make_zone(&app, "AutoPlay Detail").await;

    patch_json(
        &app,
        &format!("/api/v1/zones/{zid}"),
        json!({ "autoplay_enabled": true }),
    )
    .await;

    let (status, zone) = get(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        zone["autoplay_enabled"], true,
        "le detail doit rapporter le reglage persiste : {zone}"
    );
}

#[tokio::test]
async fn autoplay_inactif_reste_inactif() {
    // Le defaut ne doit pas basculer dans l'autre sens en corrigeant le bug.
    let app = make_app();
    let zid = make_zone(&app, "AutoPlay Defaut").await;

    let (status, zone) = get(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(zone["autoplay_enabled"], false, "defaut attendu : {zone}");
}

/// Une playlist bâtie depuis les favoris radio doit dire ce qui n'a PAS marché.
///
/// L'ancien chemin local ne rendait que `matched_tracks` : « 0 sur 2 » sans
/// indiquer lesquels, ni si la recherche avait échoué, ni si un candidat avait
/// été trouvé puis refusé au seuil. C'est exactement l'aveuglement qui a rendu
/// #1235 indiagnosticable pendant des semaines côté streaming — corrigé là-bas
/// par #1079, jamais ici.
///
/// Le test vise le rapport, pas la qualité du rapprochement : la bibliothèque
/// est vide, donc aucun favori ne peut correspondre. Ce qui doit être vrai,
/// c'est que chaque favori figure dans le compte rendu avec une raison.
#[tokio::test]
async fn playlist_depuis_favoris_radio_rend_compte_de_chaque_favori() {
    let app = make_app();

    for (title, artist) in [
        ("Nightswimming", "R.E.M."),
        ("Under the Strikes", "Sofiane Pamart"),
    ] {
        let (st, _) = post_json(
            &app,
            "/api/v1/radio-favorites",
            serde_json::json!({
                "title": title,
                "artist": artist,
                "station_name": "FIP",
            }),
        )
        .await;
        assert!(
            st.is_success(),
            "le favori « {title} » doit pouvoir être enregistré (statut {st})"
        );
    }

    let (status, body) = post_json(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        serde_json::json!({ "playlist_name": "Depuis FIP" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "réponse : {body}");
    assert_eq!(body["favorites_count"], 2);

    let results = body["results"]
        .as_array()
        .unwrap_or_else(|| panic!("le rapport par favori doit être présent : {body}"));
    assert_eq!(
        results.len(),
        2,
        "chaque favori doit apparaître dans le compte rendu, y compris ceux qui \
         n'ont rien donné — sinon l'utilisateur ne peut ni corriger un tag ni \
         signaler utilement : {body}"
    );

    for r in results {
        let s = r["status"].as_str().unwrap_or("");
        assert!(
            [
                "matched",
                "not_found",
                "rejected",
                "search_failed",
                "duplicate",
                "add_failed"
            ]
            .contains(&s),
            "statut inattendu « {s} » dans {r}"
        );
        assert!(
            r["title"].as_str().is_some_and(|t| !t.is_empty()),
            "chaque ligne doit nommer le favori concerné : {r}"
        );
    }
}

/// Le scan de doublons : la porte manquante d'un moteur qui existait.
///
/// `duplicate_detector::scan_duplicates` était complet dans `tune-core` et
/// n'avait AUCUN appelant. L'interface, elle, appelait
/// `/metadata/duplicates/scan` — un chemin qui n'a jamais existé (#1893). Ce
/// test garde les deux moitiés du contrat : la route répond, et elle rend les
/// deux champs que l'écran Métadonnées lit pour composer sa phrase de résultat.
#[tokio::test]
async fn library_duplicates_scan_repond_les_compteurs_attendus() {
    let app = make_app();
    let (status, body) = post_json(&app, "/api/v1/library/duplicates/scan", json!({})).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "la route doit exister (elle 404ait)"
    );
    assert!(
        body["total_scanned"].is_number(),
        "l'écran affiche « X doublons sur Y pistes » : total_scanned est obligatoire, reçu {body}"
    );
    assert!(
        body["duplicates_found"].is_number(),
        "duplicates_found est obligatoire, reçu {body}"
    );
}

/// Une bibliothèque vide rend zéro, pas une erreur.
///
/// L'écran distingue « aucun doublon » de « le scan a échoué » : rendre une
/// erreur sur une bibliothèque vide afficherait un échec là où il n'y a
/// simplement rien à trouver.
#[tokio::test]
async fn library_duplicates_scan_bibliotheque_vide_rend_zero() {
    let app = make_app();
    let (status, body) =
        post_json(&app, "/api/v1/library/duplicates/scan?limit=10", json!({})).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_scanned"], 0);
    assert_eq!(body["duplicates_found"], 0);
    assert_eq!(
        body["errors"], 0,
        "aucun fichier lu, donc aucune erreur de lecture"
    );
}

/// `/sonos/speakers` : la seule des quatre routes Sonos que l'interface appelle
/// réellement (`Sidebar.svelte`) ; les trois autres n'ont aucun appelant et ne
/// sont donc pas écrites.
///
/// Elle n'existait pas — la section multiroom restait vide sans rien dire.
/// `/rooms` sert les mêmes appareils mais sous d'autres noms (`id`/`host` au
/// lieu de `uid`/`ip`) : renommer la route n'aurait pas suffi, c'est la forme
/// qui diffère (#2004).
#[tokio::test]
async fn sonos_speakers_rend_un_tableau() {
    let app = make_app();
    let (status, body) = get(&app, "/api/v1/sonos/speakers").await;

    assert_eq!(
        status,
        StatusCode::OK,
        "la route doit exister (elle 404ait)"
    );
    assert!(
        body.is_array(),
        "la barre latérale fait `for sp of speakers` : un objet la casserait, reçu {body}"
    );
}

/// `/metadata/mp3/diagnose` : les compteurs que l'écran lit doivent exister.
///
/// Le contrat web laisse la liste des anomalies libre, mais `scanned`,
/// `ok_files` et `missing_files` alimentent une phrase de résultat : les
/// omettre afficherait « undefined » (#1893).
#[tokio::test]
async fn mp3_diagnose_rend_les_compteurs_du_contrat() {
    let app = make_app();
    let (status, body) = post_json(&app, "/api/v1/metadata/mp3/diagnose", json!({})).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "la route doit exister (elle 404ait)"
    );
    for champ in ["scanned", "ok_files", "missing_files", "issues_found"] {
        assert!(
            body[champ].is_number(),
            "{champ} est lu par l'écran et doit être un nombre, reçu {body}"
        );
    }
    assert!(
        body["issues"].is_array(),
        "issues doit être un tableau, reçu {body}"
    );
}

/// Réparer une liste vide ne doit rien tenter et ne pas échouer.
///
/// L'écran envoie `mp3Issues.map(i => i.track_id)` : un diagnostic sans
/// anomalie produit une liste vide, cas normal et non une erreur.
#[tokio::test]
async fn mp3_repair_liste_vide_ne_fait_rien() {
    let app = make_app();
    let (status, body) = post_json(
        &app,
        "/api/v1/metadata/mp3/repair",
        json!({"track_ids": []}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["repaired"], 0);
    assert_eq!(body["requested"], 0);
    assert!(body["failed"].is_array());
}

/// Une piste inconnue est un ÉCHEC nommé, pas un silence.
///
/// L'écran affiche `failed.length` : avaler l'identifiant inconnu ferait
/// croire à une réparation réussie sur une piste qui n'existe pas.
#[tokio::test]
async fn mp3_repair_piste_inconnue_est_un_echec_explicite() {
    let app = make_app();
    let (status, body) = post_json(
        &app,
        "/api/v1/metadata/mp3/repair",
        json!({"track_ids": [999_999_999]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["repaired"], 0);
    assert_eq!(body["requested"], 1);
    assert_eq!(
        body["failed"].as_array().map(|a| a.len()),
        Some(1),
        "l'identifiant inconnu doit apparaître dans failed, reçu {body}"
    );
}
