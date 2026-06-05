//! Axum HTTP handlers for the UPnP MediaServer (ContentDirectory).
//!
//! The SOAP parsing, DIDL-Lite generation, and SSDP helpers live in
//! `tune_core::upnp_server`. This module provides the Axum route layer only.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use tune_core::upnp_server::UpnpState;

pub fn router() -> Router<UpnpState> {
    Router::new()
        .route("/description.xml", get(device_description))
        .route("/ContentDirectory/control", post(content_directory_control))
        .route("/ContentDirectory/event", get(content_directory_event))
        .route(
            "/ConnectionManager/control",
            post(connection_manager_control),
        )
}

/// Build a standalone Axum `Router` (with state already applied) suitable for
/// merging into the main server or serving separately.
pub fn standalone_router(state: UpnpState) -> Router {
    router().with_state(state)
}

async fn device_description(State(state): State<UpnpState>) -> impl IntoResponse {
    let xml = tune_core::upnp_server::build_device_description(&state);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

async fn content_directory_control(
    State(state): State<UpnpState>,
    body: String,
) -> impl IntoResponse {
    let soap = tune_core::upnp_server::build_browse_response(&state, &body);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(soap))
        .unwrap()
}

async fn content_directory_event() -> impl IntoResponse {
    StatusCode::OK
}

async fn connection_manager_control(body: String) -> impl IntoResponse {
    let soap = tune_core::upnp_server::build_connection_manager_response(&body);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(soap))
        .unwrap()
}
