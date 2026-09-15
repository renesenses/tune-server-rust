//! Real plugin wiring, no network session and no audio device.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;
use tune_core::{db::settings_repo::SettingsRepo, plugin_sdk::PluginLoader};
use tune_server::state::AppState;

async fn fixture(disabled: bool) -> (AppState, tempfile::TempDir) {
    let config = tune_server::config::TuneConfig {
        spotify_client_id: Some("fixture-client-not-a-secret".into()),
        ..Default::default()
    };
    let state = AppState::new(":memory:", 19191, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    *state.plugins.lock().await = PluginLoader::new(dir.path().into())
        .with_db(state.backend.clone())
        .with_event_bus((*state.event_bus).clone());
    if disabled {
        SettingsRepo::with_backend(state.backend.clone())
            .set("plugin_spotify_enabled", "false")
            .unwrap();
    }
    (state, dir)
}

async fn response(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn source_only_exists_after_plugin_setup_and_restores_its_enabled_state() {
    let (state, _dir) = fixture(false).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("streaming_spotify_enabled", "false").unwrap();
    let tidal = state.services.lock().await.get("tidal").unwrap();
    tidal.write().await.set_enabled(false);
    settings.set("streaming_tidal_enabled", "true").unwrap();
    settings
        .set("auth_tokens_spotify_native", "{\"private-fixture\":true}")
        .unwrap();
    assert!(
        state.services.lock().await.get("spotify").is_none(),
        "Spotify must not be built into AppState"
    );
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:19191", vec![]).await;
    assert!(
        !tidal.read().await.enabled(),
        "installing a plugin must not restore existing services a second time"
    );
    let service = state
        .services
        .lock()
        .await
        .get("spotify")
        .expect("plugin service missing");
    assert!(
        !service.read().await.enabled(),
        "late plugin registration must restore saved enabled state"
    );
    assert_eq!(service.read().await.credential_key(), "auth_tokens_spotify");
    assert_eq!(
        settings
            .get("auth_tokens_spotify_native")
            .unwrap()
            .as_deref(),
        Some("{\"private-fixture\":true}"),
        "OAuth mode must not touch native pairing"
    );
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    let legacy = response(&app, "/api/v1/spotify-connect/status").await;
    let plugin = response(&app, "/api/v1/ext/spotify/status").await;
    assert_eq!(legacy.0, StatusCode::OK);
    assert_eq!(
        legacy, plugin,
        "legacy Connect route must use the same plugin state"
    );
    state.plugins.lock().await.teardown_all().await;
}

#[tokio::test]
async fn disabled_plugin_exposes_neither_source_nor_routes_and_keeps_credentials() {
    let (state, _dir) = fixture(true).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set("auth_tokens_spotify", "{\"unread-fixture\":true}")
        .unwrap();
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:19191", vec![]).await;
    assert!(state.services.lock().await.get("spotify").is_none());
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    for path in [
        "/api/v1/ext/spotify/status",
        "/api/v1/spotify-connect/status",
        "/api/v1/streaming/spotify/native-pairing",
        "/api/v1/streaming/spotify/native-pairing.js",
        "/api/v1/streaming/spotify/callback",
    ] {
        assert_eq!(
            response(&app, path).await.0,
            StatusCode::NOT_FOUND,
            "disabled plugin must not retain {path}"
        );
    }
    assert_eq!(
        settings.get("auth_tokens_spotify").unwrap().as_deref(),
        Some("{\"unread-fixture\":true}")
    );
}

#[tokio::test]
async fn both_plugin_and_compatibility_routes_remain_authenticated() {
    let (state, _dir) = fixture(false).await;
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:19191", vec![]).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings
        .set("jwt_secret", "test-only-not-a-secret")
        .unwrap();
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    for path in [
        "/api/v1/ext/spotify/status",
        "/api/v1/spotify-connect/status",
        "/api/v1/streaming/spotify/native-pairing",
        "/api/v1/streaming/spotify/native-pairing.js",
    ] {
        assert_eq!(response(&app, path).await.0, StatusCode::UNAUTHORIZED);
    }
    state.plugins.lock().await.teardown_all().await;
}

#[cfg(feature = "spotify-native")]
#[tokio::test]
async fn native_onboarding_aliases_do_not_shadow_generic_service_routes() {
    let (state, _dir) = fixture(false).await;
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:19191", vec![]).await;
    state.services.lock().await.register(Box::new(
        tune_spotify::spotify_native::SpotifyNativeService::new(),
    ));
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    for path in [
        "/api/v1/streaming/spotify/native-pairing",
        "/api/v1/ext/spotify/native-pairing",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "native onboarding missing at {path}"
        );
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }
    let script = app
        .clone()
        .oneshot(
            Request::get("/api/v1/streaming/spotify/native-pairing.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(script.status(), StatusCode::OK);
    assert_eq!(
        script.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    let status = response(&app, "/api/v1/streaming/spotify/status").await;
    assert_eq!(status.0, StatusCode::OK);
    assert!(
        status.1["authenticated"].is_boolean(),
        "onboarding alias must not shadow the generic service status: {}",
        status.1
    );
    assert!(state.streamer.sessions_state().lock().await.is_empty());
    state.plugins.lock().await.teardown_all().await;
}

#[tokio::test]
async fn oauth_callback_retains_its_legacy_contract_without_authenticating_on_get_errors() {
    let (state, _dir) = fixture(false).await;
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:19191", vec![]).await;
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    let legacy = response(
        &app,
        "/api/v1/streaming/spotify/callback?error=access_denied",
    )
    .await;
    assert_eq!(legacy.0, StatusCode::OK);
    assert_eq!(legacy.1, serde_json::json!({"error":"access_denied"}));
    assert_eq!(
        response(&app, "/api/v1/ext/spotify/callback?error=access_denied").await,
        legacy
    );
    assert_eq!(
        response(&app, "/api/v1/streaming/spotify/callback").await.0,
        StatusCode::BAD_REQUEST
    );
    state.plugins.lock().await.teardown_all().await;
}

#[cfg(feature = "spotify-native")]
#[tokio::test]
async fn native_worker_and_audio_capability_are_contributed_by_the_plugin() {
    use tune_core::streaming::StreamingService;
    let mut service = tune_spotify::spotify_native::SpotifyNativeService::new();
    assert!(service.private_audio());
    assert_eq!(service.credential_key(), "auth_tokens_spotify_native");
    service.shutdown().await;
    assert!(!service.enabled());
    let entries = tune_server::plugins::builtin_workers();
    assert_eq!(
        tune_core::plugin_worker::dispatch(
            &entries,
            vec!["--spotify-native-worker".into(), "invalid".into()]
        )
        .await
        .unwrap(),
        Some(1)
    );
}
