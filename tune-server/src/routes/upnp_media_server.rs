//! Axum HTTP handlers for the UPnP MediaServer (ContentDirectory).
//!
//! The SOAP parsing, DIDL-Lite generation, and SSDP helpers live in
//! `tune_core::upnp_server`. This module provides the Axum route layer only.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};

use tune_core::upnp_server::UpnpState;

pub fn router() -> Router<UpnpState> {
    Router::new()
        .route("/description.xml", get(device_description))
        .route("/ContentDirectory/control", post(content_directory_control))
        // GENA : SUBSCRIBE/UNSUBSCRIBE sont des méthodes HTTP custom — `any`
        // les accepte là où `get` répondait 405 et faisait échouer des points
        // de contrôle stricts avant leur premier Browse.
        .route("/ContentDirectory/event", any(event_subscription))
        .route("/ContentDirectory/scpd.xml", get(content_directory_scpd))
        .route(
            "/ConnectionManager/control",
            post(connection_manager_control),
        )
        // Annoncée par le description.xml mais jamais routée : un SUBSCRIBE
        // dessus tombait sur le fallback SPA.
        .route("/ConnectionManager/event", any(event_subscription))
        .route("/ConnectionManager/scpd.xml", get(connection_manager_scpd))
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

/// Réponse SOAP → HTTP : la spec UPnP impose 500 pour un fault.
fn soap_response(soap: String) -> Response {
    let status = if tune_core::upnp_server::is_soap_fault(&soap) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(soap))
        .unwrap()
}

async fn content_directory_control(
    State(state): State<UpnpState>,
    body: String,
) -> impl IntoResponse {
    soap_response(tune_core::upnp_server::build_browse_response(&state, &body))
}

/// GENA minimal : on accepte l'abonnement (SID + TIMEOUT) sans conserver
/// d'état — le serveur n'émet pas d'événements, mais un SUBSCRIBE refusé
/// suffit à faire abandonner certains clients (JPLAY).
async fn event_subscription(method: Method) -> Response {
    match method.as_str() {
        "SUBSCRIBE" => Response::builder()
            .status(StatusCode::OK)
            .header("SID", tune_core::upnp_server::new_subscription_sid())
            .header("TIMEOUT", "Second-1800")
            .body(Body::empty())
            .unwrap(),
        // UNSUBSCRIBE, GET (sonde), HEAD… : 200 sans corps.
        _ => Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap(),
    }
}

/// SCPD du ContentDirectory. Sans ces routes, la SCPDURL publiée par le
/// description.xml tombait sur le fallback SPA (du HTML en 200) — les points
/// de contrôle stricts qui parsent le SCPD refusaient le serveur (#1613).
async fn content_directory_scpd() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(tune_core::upnp_server::content_directory_scpd()))
        .unwrap()
}

async fn connection_manager_scpd() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(tune_core::upnp_server::connection_manager_scpd()))
        .unwrap()
}

async fn connection_manager_control(body: String) -> impl IntoResponse {
    soap_response(tune_core::upnp_server::build_connection_manager_response(
        &body,
    ))
}
