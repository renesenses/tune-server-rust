use crate::connect_routes::SpotifyHttpState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

async fn get_svc(
    state: &SpotifyHttpState,
    name: &str,
) -> Result<tune_core::streaming::registry::ServiceHandle, (StatusCode, String)> {
    state
        .services
        .lock()
        .await
        .get(name)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("service not found: {name}")))
}

/// Server-owned onboarding for clients that already open verification_url.
/// GET only displays status; pairing requires an explicit POST.
pub(super) async fn page(State(state): State<SpotifyHttpState>) -> Response {
    let service = match get_svc(&state, "spotify").await {
        Ok(service) => service,
        Err(error) => return error.into_response(),
    };
    if service
        .read()
        .await
        .auth_details()
        .as_ref()
        .and_then(|d| d["mode"].as_str())
        != Some("unofficial-native")
    {
        return (
            StatusCode::NOT_FOUND,
            "Native Spotify prototype is not enabled",
        )
            .into_response();
    }
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
            ("x-content-type-options", "nosniff"),
            ("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'"),
        ],
        axum::response::Html(include_str!("spotify_pairing.html")),
    ).into_response()
}

pub(super) async fn script() -> Response {
    (
        [
            ("content-type", "text/javascript; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        include_str!("spotify_pairing.js"),
    )
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct SpotifyCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub(super) async fn callback(
    State(state): State<SpotifyHttpState>,
    Query(q): Query<SpotifyCallbackQuery>,
) -> Response {
    if let Some(ref error) = q.error {
        return Json(json!({"error": error})).into_response();
    }
    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing code parameter").into_response();
    };
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;
    match svc
        .authenticate(&json!({"code": code, "state": q.state}))
        .await
    {
        Ok(status) => {
            drop(svc);
            state
                .services
                .lock()
                .await
                .save_all_tokens(&state.backend)
                .await;
            (state.invalidate_content)("spotify");
            Json(json!({
                "authenticated": status.authenticated,
                "username": status.username,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> SpotifyHttpState {
        use std::sync::Arc;
        SpotifyHttpState {
            backend: Arc::new(tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap()),
            services: Arc::new(tokio::sync::Mutex::new(
                tune_core::streaming::ServiceRegistry::new(),
            )),
            spotify_connect: Arc::new(crate::spotify_connect::SpotifyConnectManager::new(
                "Fixture".into(),
                0,
            )),
            http_client: tune_core::http::client::builder().build().unwrap(),
            invalidate_content: |_| {},
        }
    }
    #[tokio::test]
    async fn native_pairing_page_does_not_exist_without_the_native_service() {
        assert_eq!(page(State(state())).await.status(), StatusCode::NOT_FOUND);
    }
    #[tokio::test]
    async fn native_pairing_script_is_served_as_javascript() {
        let response = script().await;
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}
