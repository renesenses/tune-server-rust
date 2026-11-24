//! #9174 — le serveur sait qu'une zone navigateur est tirée par le relais.
//!
//! Le lecteur d'une zone navigateur (« Ce téléphone ») signale sa lecture
//! toutes les 5 s. Par le vrai routeur : le même signal, avec puis sans la
//! marque du relais, fait passer la zone à distance puis la ramène en local.
//! C'est cette provenance qui fait servir du FLAC indexé plutôt que du WAV.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use tower::ServiceExt;

use tune_core::cloud::ENTETE_RELAIS;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

async fn appel(app: &Router, methode: Method, chemin: &str, relaye: bool) {
    let mut req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header(header::CONTENT_TYPE, "application/json");
    if relaye {
        req = req.header(ENTETE_RELAIS, "1");
    }
    let mut requete = req.body(Body::from("{}")).unwrap();
    let pair: SocketAddr = "127.0.0.1:50124".parse().unwrap();
    requete.extensions_mut().insert(ConnectInfo(pair));
    let _ = app.clone().oneshot(requete).await.unwrap();
}

#[tokio::test]
async fn le_lecteur_tire_par_le_relais_met_sa_zone_a_distance() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Ce téléphone", Some("browser"), None)
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    let signal = format!("/api/v1/zones/{zone}/browser-playing");

    appel(&app, Method::POST, &signal, false).await;
    assert!(!state.orchestrator.zone_tiree_a_distance(zone));

    appel(&app, Method::POST, &signal, true).await;
    assert!(
        state.orchestrator.zone_tiree_a_distance(zone),
        "un signal relayé doit mettre la zone à distance"
    );

    // Une télécommande locale qui lit l'état ne ramène pas le lecteur.
    appel(
        &app,
        Method::GET,
        &format!("/api/v1/zones/{zone}/status"),
        false,
    )
    .await;
    assert!(state.orchestrator.zone_tiree_a_distance(zone));

    appel(&app, Method::POST, &signal, false).await;
    assert!(
        !state.orchestrator.zone_tiree_a_distance(zone),
        "le lecteur revenu en local ramène la zone en local"
    );
}
