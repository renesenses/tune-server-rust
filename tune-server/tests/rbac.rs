//! Security regression test for admin RBAC (audit item 4). The JWT carries a
//! role, but before the fix no business route checked it — any valid token
//! could wipe the library, mutate config, restore a backup, export the DB or
//! trigger a self-update. `RequireAdmin` now gates those.
//!
//! `POST /system/library/clear` is used as the probe: it is admin-gated and, on
//! an in-memory DB, harmless (deletes zero rows).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret";
const ADMIN_ROUTE: &str = "/api/v1/system/library/clear";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
}

fn tok(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET).unwrap()
}

async fn clear_status(state: &AppState, bearer: Option<&str>) -> StatusCode {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::post(ADMIN_ROUTE);
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    app.oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn admin_route_open_when_auth_disabled() {
    let state = new_state();
    // Auth off → server is open, admin gate passes through.
    assert_eq!(clear_status(&state, None).await, StatusCode::OK);
}

#[tokio::test]
async fn admin_route_forbidden_for_user_token() {
    let state = new_state();
    enable_auth(&state);
    // Valid token, but role=user → 403 (this is the core of the fix).
    assert_eq!(
        clear_status(&state, Some(&tok("user", 2))).await,
        StatusCode::FORBIDDEN,
    );
}

#[tokio::test]
async fn admin_route_allowed_for_admin_token() {
    let state = new_state();
    enable_auth(&state);
    assert_eq!(
        clear_status(&state, Some(&tok("admin", 1))).await,
        StatusCode::OK,
    );
}

#[tokio::test]
async fn admin_route_unauthorized_without_token() {
    let state = new_state();
    enable_auth(&state);
    assert_eq!(clear_status(&state, None).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_route_unauthorized_with_invalid_token() {
    let state = new_state();
    enable_auth(&state);
    assert_eq!(
        clear_status(&state, Some("not-a-valid-jwt")).await,
        StatusCode::UNAUTHORIZED,
    );
}

/// A representative route from the *extended* set (DB maintenance) — proves the
/// `RequireAdmin` annotation was actually applied to the second batch, not just
/// library_clear.
#[tokio::test]
async fn extended_admin_route_forbidden_for_user_token() {
    let state = new_state();
    enable_auth(&state);
    let app: Router = tune_server::routes::router(state.clone());
    let req = Request::post("/api/v1/system/database/optimize")
        .header(header::AUTHORIZATION, format!("Bearer {}", tok("user", 2)))
        .body(Body::empty())
        .unwrap();
    let st = app.oneshot(req).await.unwrap().status();
    assert_eq!(st, StatusCode::FORBIDDEN);
}
