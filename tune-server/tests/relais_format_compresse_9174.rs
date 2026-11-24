//! #9174 — le serveur sait qu'une zone navigateur est tirée par le relais.
//!
//! Le lecteur d'une zone navigateur (« Ce téléphone ») signale sa lecture
//! toutes les 5 s. Par le vrai routeur : le même signal, avec puis sans la
//! marque du relais, fait passer la zone à distance puis la ramène en local.
//! Rien d'autre ne compte : ni une télécommande, ni une requête refusée
//! (authentification, corps invalide), ni une zone qui n'est pas un
//! navigateur.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::cloud::ENTETE_RELAIS;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

const SIGNAL: &str = r#"{"stream_id":"abc","position_ms":0}"#;

async fn appel(app: &Router, methode: Method, chemin: &str, relaye: bool, corps: &str) -> StatusCode {
    let mut req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header(header::CONTENT_TYPE, "application/json");
    if relaye {
        req = req.header(ENTETE_RELAIS, "1");
    }
    let mut requete = req.body(Body::from(corps.to_string())).unwrap();
    let pair: SocketAddr = "127.0.0.1:50124".parse().unwrap();
    requete.extensions_mut().insert(ConnectInfo(pair));
    app.clone().oneshot(requete).await.unwrap().status()
}

fn etat() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Ce téléphone", Some("browser"), None)
        .unwrap();
    (state, zone)
}

#[tokio::test]
async fn le_lecteur_tire_par_le_relais_met_sa_zone_a_distance() {
    let (state, zone) = etat();
    let app = tune_server::routes::router(state.clone());
    let signal = format!("/api/v1/zones/{zone}/browser-playing");

    let s = appel(&app, Method::POST, &signal, false, SIGNAL).await;
    assert!(s.is_success(), "{s}");
    assert!(!state.orchestrator.zone_tiree_a_distance(zone));

    let s = appel(&app, Method::POST, &signal, true, SIGNAL).await;
    assert!(s.is_success(), "{s}");
    assert!(
        state.orchestrator.zone_tiree_a_distance(zone),
        "un signal relayé doit mettre la zone à distance"
    );

    // Une télécommande locale qui lit l'état ne ramène pas le lecteur.
    appel(&app, Method::GET, &format!("/api/v1/zones/{zone}/status"), false, "").await;
    assert!(state.orchestrator.zone_tiree_a_distance(zone));

    let s = appel(&app, Method::POST, &signal, false, SIGNAL).await;
    assert!(s.is_success(), "{s}");
    assert!(
        !state.orchestrator.zone_tiree_a_distance(zone),
        "le lecteur revenu en local ramène la zone en local"
    );
}

#[tokio::test]
async fn une_telecommande_relayee_ne_met_pas_la_zone_a_distance() {
    let (state, zone) = etat();
    let app = tune_server::routes::router(state.clone());
    appel(&app, Method::GET, &format!("/api/v1/zones/{zone}/status"), true, "").await;
    appel(&app, Method::GET, &format!("/api/v1/zones/{zone}/queue"), true, "").await;
    assert!(!state.orchestrator.zone_tiree_a_distance(zone));
}

#[tokio::test]
async fn une_requete_refusee_ne_compte_pas() {
    let (state, zone) = etat();
    let app = tune_server::routes::router(state.clone());
    let signal = format!("/api/v1/zones/{zone}/browser-playing");
    // Corps invalide : refusé, rien n'est noté.
    let s = appel(&app, Method::POST, &signal, true, "{}").await;
    assert!(!s.is_success(), "{s}");
    assert!(!state.orchestrator.zone_tiree_a_distance(zone));

    // Une zone qui n'est pas un navigateur.
    let dlna = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("uuid:salon"))
        .unwrap();
    appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{dlna}/browser-playing"),
        true,
        SIGNAL,
    )
    .await;
    assert!(!state.orchestrator.zone_tiree_a_distance(dlna));

    // Une zone qui n'existe pas.
    appel(&app, Method::POST, "/api/v1/zones/987654/browser-playing", true, SIGNAL).await;
    assert!(!state.orchestrator.zone_tiree_a_distance(987654));
}

#[tokio::test]
async fn sans_authentification_rien_n_est_note() {
    let (state, zone) = etat();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", "test-jwt-secret").unwrap();
    let app = tune_server::routes::router(state.clone());
    let s = appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{zone}/browser-playing"),
        true,
        SIGNAL,
    )
    .await;
    assert!(!s.is_success(), "sans jeton, refusé : {s}");
    assert!(!state.orchestrator.zone_tiree_a_distance(zone));
}
