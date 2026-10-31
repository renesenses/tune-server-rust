//! Une requête rejouée par le relais (en-tête `x-tune-relais`, posé par le
//! relais et l'écoute directe) n'est pas une requête locale, et n'atteint que
//! les routes d'écoute.
//!
//! Chaque épreuve compare la même requête avec et sans l'en-tête : sans lui,
//! depuis 127.0.0.1, la route répond ; avec lui, elle est refusée. C'est
//! l'en-tête, et rien d'autre, qui change la réponse.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::cloud::ENTETE_RELAIS;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const LOCAL: &str = "127.0.0.1:50123";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", "test-jwt-secret").unwrap();
}

fn app(state: &AppState, pair: &str) -> Router {
    let pair: SocketAddr = pair.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair))
}

async fn appel(
    app: &Router,
    methode: Method,
    chemin: &str,
    relaye: bool,
    corps: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if relaye {
        req = req.header(ENTETE_RELAIS, "1");
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&octets).to_string())
}

/// Authentification coupée (le défaut) : l'administration ne passe plus par
/// le relais, la bibliothèque et la lecture si.
#[tokio::test]
async fn sans_authentification_le_relais_ne_sert_que_l_ecoute() {
    let state = new_state();
    let app = app(&state, LOCAL);

    for (m, chemin, corps) in [
        (Method::GET, "/api/v1/system/config", None),
        (Method::PATCH, "/api/v1/system/config", Some(r#"{"server_name":"x"}"#)),
        (Method::GET, "/api/v1/system/config/export?include_secrets=true", None),
        (Method::GET, "/api/v1/cloud/bridge/status", None),
        (Method::GET, "/api/v1/auth/config", None),
        (Method::GET, "/api/v1/system/logs", None),
    ] {
        let (local, _) = appel(&app, m.clone(), chemin, false, corps).await;
        assert_ne!(
            local,
            StatusCode::FORBIDDEN,
            "{m} {chemin} sans relais devait répondre (témoin)"
        );
        let (relaye, corps_rep) = appel(&app, m.clone(), chemin, true, corps).await;
        assert_eq!(
            relaye,
            StatusCode::FORBIDDEN,
            "{m} {chemin} par le relais doit être refusé : {corps_rep}"
        );
        assert!(corps_rep.contains("not_available_remotely"), "{corps_rep}");
    }

    // PATCH refusé : le réglage n'a pas bougé.
    let nom = SettingsRepo::with_backend(state.backend.clone())
        .get("server_name")
        .unwrap();
    assert_ne!(nom.as_deref(), Some("x"), "le PATCH relayé a écrit");

    for chemin in [
        "/api/v1/library/albums",
        "/api/v1/library/artists",
        "/api/v1/zones/",
        "/api/v1/playlists/",
        "/api/v1/system/health",
    ] {
        let (st, corps) = appel(&app, Method::GET, chemin, true, None).await;
        assert_eq!(st, StatusCode::OK, "{chemin} par le relais : {corps}");
    }
}

/// Authentification active : la limite de tentatives s'applique au relais,
/// et le profil `default` sans mot de passe n'y est pas revendicable.
#[tokio::test]
async fn avec_authentification_le_relais_n_est_pas_local() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, LOCAL);

    // `default` sans mot de passe : refusé par le relais.
    let (st, corps) = appel(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        true,
        Some(r#"{"username":"default","password":"pris-par-le-relais"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "default revendiqué par le relais : {corps}");

    // Force brute : 9 échecs de plus (10 au total), puis 429.
    for _ in 0..9 {
        let (st, _) = appel(
            &app,
            Method::POST,
            "/api/v1/auth/login",
            true,
            Some(r#"{"username":"nope","password":"bad"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let (st, _) = appel(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        true,
        Some(r#"{"username":"nope","password":"bad"}"#),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::TOO_MANY_REQUESTS,
        "le relais doit subir la limite de tentatives"
    );

    // Témoin : depuis 127.0.0.1 sans relais, l'opérateur local n'est pas
    // verrouillé et revendique `default` au premier passage.
    let (st, corps) = appel(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        false,
        Some(r#"{"username":"default","password":"premier-passage"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "opérateur local : {corps}");
}

/// Authentification active : l'exemption DIDL (renderer du réseau local sans
/// jeton) ne s'ouvre pas à une requête relayée.
#[tokio::test]
async fn l_exemption_didl_ne_s_applique_pas_au_relais() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, LOCAL);
    let chemin = "/api/v1/library/tracks/424242/audio";

    let (local, _) = appel(&app, Method::GET, chemin, false, None).await;
    assert_ne!(local, StatusCode::UNAUTHORIZED, "témoin : exemption DIDL en local");
    let (relaye, _) = appel(&app, Method::GET, chemin, true, None).await;
    assert_eq!(relaye, StatusCode::UNAUTHORIZED, "exemption DIDL ouverte au relais");
}
