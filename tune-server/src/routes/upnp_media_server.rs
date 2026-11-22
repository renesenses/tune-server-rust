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

#[path = "upnp_events.rs"]
mod events;

pub fn router() -> Router<UpnpState> {
    Router::new()
        .route("/description.xml", get(device_description))
        .route("/ContentDirectory/control", post(content_directory_control))
        // GENA : SUBSCRIBE/UNSUBSCRIBE sont des méthodes HTTP custom — `any`
        // les accepte là où `get` répondait 405 et faisait échouer des points
        // de contrôle stricts avant leur premier Browse.
        .route("/ContentDirectory/event", any(events::subscription))
        .route("/ContentDirectory/scpd.xml", get(content_directory_scpd))
        .route(
            "/ConnectionManager/control",
            post(connection_manager_control),
        )
        // Annoncée par le description.xml mais jamais routée : un SUBSCRIBE
        // dessus tombait sur le fallback SPA.
        .route("/ConnectionManager/event", any(event_subscription))
        .route("/ConnectionManager/scpd.xml", get(connection_manager_scpd))
        .layer(axum::Extension(std::sync::Arc::new(
            events::Registry::default(),
        )))
}

/// Build a standalone Axum `Router` (with state already applied) suitable for
/// merging into the main server or serving separately.
///
/// #9079 — toutes les routes du MediaServer passent par
/// [`serveur_coupe_en_404`] : serveur multimédia désactivé, elles répondent
/// 404 au lieu de servir encore la description et le catalogue.
pub fn standalone_router(state: UpnpState) -> Router {
    router()
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            serveur_coupe_en_404,
        ))
        .with_state(state)
}

/// #9079 — un point de contrôle qui a déjà vu Tune (JPLAY) le revalide par
/// un GET de sa LOCATION. Tant que la description répondait, il le gardait
/// dans sa liste même annonce coupée.
async fn serveur_coupe_en_404(
    State(state): State<UpnpState>,
    requete: axum::extract::Request,
    suite: axum::middleware::Next,
) -> Response {
    if !state.annonce.est_actif() {
        return StatusCode::NOT_FOUND.into_response();
    }
    suite.run(requete).await
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

/// Abonnement historique ConnectionManager, sans notification de ses variables.
/// Les événements du catalogue sont gérés par `events::subscription`.
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

#[cfg(test)]
mod serveur_coupe_9079_tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_core::db::backend::DbBackend;
    use tune_core::upnp_server::UpnpState;

    fn etat(upnp_enabled: Option<&str>) -> UpnpState {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        if let Some(v) = upnp_enabled {
            tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
                .set("upnp_enabled", v)
                .unwrap();
        }
        UpnpState::new(backend, 8888, None)
    }

    async fn statut(state: &UpnpState, methode: &str, chemin: &str) -> StatusCode {
        super::standalone_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(methode)
                    .uri(chemin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    const ROUTES: &[(&str, &str)] = &[
        ("GET", "/description.xml"),
        ("POST", "/ContentDirectory/control"),
        ("SUBSCRIBE", "/ContentDirectory/event"),
        ("GET", "/ContentDirectory/scpd.xml"),
        ("POST", "/ConnectionManager/control"),
        ("SUBSCRIBE", "/ConnectionManager/event"),
        ("GET", "/ConnectionManager/scpd.xml"),
    ];

    /// Le symptôme : `upnp_enabled=false`, la description répondait encore.
    #[tokio::test]
    async fn serveur_coupe_toutes_les_routes_repondent_404() {
        let state = etat(Some("false"));
        for (methode, chemin) in ROUTES {
            assert_eq!(
                statut(&state, methode, chemin).await,
                StatusCode::NOT_FOUND,
                "{methode} {chemin} doit repondre 404 quand le serveur multimedia est coupe"
            );
        }
    }

    /// Témoin : en service (réglage absent ou à true), la description et le
    /// SCPD répondent 200 — la garde ne coupe pas un serveur actif.
    #[tokio::test]
    async fn serveur_en_service_la_description_repond() {
        for reglage in [None, Some("true")] {
            let state = etat(reglage);
            assert_eq!(
                statut(&state, "GET", "/description.xml").await,
                StatusCode::OK
            );
            assert_eq!(
                statut(&state, "GET", "/ContentDirectory/scpd.xml").await,
                StatusCode::OK
            );
        }
    }

    /// La bascule prend effet sans redémarrage, dans les deux sens.
    #[tokio::test]
    async fn la_bascule_prend_effet_sans_redemarrage() {
        let state = etat(None);
        assert_eq!(
            statut(&state, "GET", "/description.xml").await,
            StatusCode::OK
        );
        state.annonce.basculer(false);
        assert_eq!(
            statut(&state, "GET", "/description.xml").await,
            StatusCode::NOT_FOUND
        );
        state.annonce.basculer(true);
        assert_eq!(
            statut(&state, "GET", "/description.xml").await,
            StatusCode::OK
        );
    }

    /// Garde de câblage : POST /api/v1/upnp/config bascule l'interrupteur et
    /// envoie le byebye quand on coupe.
    #[test]
    fn l_ecran_de_configuration_bascule_et_retire_du_reseau() {
        let ecran = include_str!("upnp.rs");
        assert!(ecran.contains("upnp.annonce.basculer(enabled)"));
        assert!(ecran.contains("tune_core::upnp_server::retirer_du_reseau().await"));
    }
}
