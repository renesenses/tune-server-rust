//! Security regression test: the WebSocket endpoints are mounted outside the
//! API auth middleware, so the upgrade itself must be gated. Before the fix an
//! anonymous socket received the full snapshot (zones, queue, now-playing) and
//! the live event stream even with auth enabled.
//!
//! Note on statuses: a synthetic `oneshot` request has no hyper upgrade state,
//! so axum's `WebSocketUpgrade` extractor can never complete the handshake in a
//! unit test — an *authorized* request therefore stops at `426 Upgrade
//! Required` (it passed the auth gate and reached the upgrade step; a real
//! connection would return `101`). An *unauthorized* request is rejected by the
//! leading `WsAuthorized` extractor with `401` before the upgrade is attempted.
//! So: 401 == gate rejected, 426 == gate passed.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
}

fn token() -> String {
    tune_server::auth::sign_jwt(1, "admin", SECRET).unwrap()
}

/// A well-formed WebSocket upgrade request (short of the live connection state
/// that only a real server provides).
fn ws_req(path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap()
}

async fn status(state: &AppState, req: Request<Body>) -> StatusCode {
    let app: Router = tune_server::routes::router(state.clone());
    app.oneshot(req).await.unwrap().status()
}

/// 426 == the auth gate passed and the request reached the upgrade step.
const GATE_PASSED: StatusCode = StatusCode::UPGRADE_REQUIRED;

#[tokio::test]
async fn ws_open_when_auth_disabled() {
    let state = new_state();
    // Default: auth off → gate passes (real server would upgrade to 101).
    assert_eq!(status(&state, ws_req("/ws")).await, GATE_PASSED);
}

#[tokio::test]
async fn ws_rejected_without_token_when_auth_enabled() {
    let state = new_state();
    enable_auth(&state);
    assert_eq!(
        status(&state, ws_req("/ws")).await,
        StatusCode::UNAUTHORIZED,
        "anonymous WS upgrade must be refused when auth is enabled"
    );
}

#[tokio::test]
async fn ws_accepts_bearer_header() {
    let state = new_state();
    enable_auth(&state);
    let mut req = ws_req("/ws");
    req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", token()).parse().unwrap(),
    );
    assert_eq!(status(&state, req).await, GATE_PASSED);
}

#[tokio::test]
async fn ws_accepts_session_cookie() {
    let state = new_state();
    enable_auth(&state);
    let mut req = ws_req("/ws");
    req.headers_mut().insert(
        header::COOKIE,
        format!("tune_session={}", token()).parse().unwrap(),
    );
    assert_eq!(status(&state, req).await, GATE_PASSED);
}

#[tokio::test]
async fn ws_accepts_query_token() {
    let state = new_state();
    enable_auth(&state);
    let path = format!("/ws?token={}", token());
    assert_eq!(status(&state, ws_req(&path)).await, GATE_PASSED);
}

#[tokio::test]
async fn ws_rejects_invalid_token() {
    let state = new_state();
    enable_auth(&state);
    let mut req = ws_req("/ws");
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "Bearer not-a-valid-jwt".parse().unwrap(),
    );
    assert_eq!(status(&state, req).await, StatusCode::UNAUTHORIZED);
}
