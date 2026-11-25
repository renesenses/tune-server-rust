//! Première configuration selon l'environnement : appliance Tune OS avec le
//! relais du port 80, conteneur dont la passerelle relaie les connexions.
//!
//! Ces cas posent des variables d'environnement lues par tout le processus :
//! UNE seule épreuve dans ce binaire, qui les enchaîne.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::auth::{ouvrir_la_premiere_configuration, premiere_configuration_restante};
use tune_server::state::AppState;

fn nouvel_etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

async fn appel(
    state: &AppState,
    adresse: &str,
    methode: Method,
    entetes: &[(&str, &str)],
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder()
        .method(methode)
        .uri("/api/v1/auth/activation");
    for (n, v) in entetes {
        req = req.header(*n, *v);
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let st = resp.status();
    let o = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&o)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&o).to_string()));
    (st, v)
}

const CORPS: &str = r#"{"password":"intrus-1234"}"#;

#[tokio::test]
async fn appliance_et_conteneur() {
    // --- Tune OS, relais du port 80 : toute connexion arrive de 127.0.0.1.
    // SAFETY: seule épreuve de ce binaire ; rien d'autre ne lit ces variables.
    unsafe {
        std::env::set_var("TUNE_APPLIANCE", "1");
        std::env::set_var("TUNE_WEB80_RELAIS", "1");
    }
    // La fenêtre ne s'ouvre pas.
    let state = nouvel_etat();
    assert!(!ouvrir_la_premiere_configuration(&state));
    assert_eq!(premiere_configuration_restante(&state), None);
    // Une connexion par le port 80, quelle qu'en soit l'origine : le code.
    let (st, v) = appel(
        &state,
        "127.0.0.1:41234",
        Method::POST,
        &[("host", "[2001:db8::42]")],
        Some(CORPS),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "tune_os_port_80_internet_vu_comme_lan {v}"
    );
    assert_eq!(v["error"], "activation_code_required", "{v}");
    assert!(!active(&state));
    let (_, v) = appel(&state, "127.0.0.1:41234", Method::GET, &[], None).await;
    assert_eq!(v["first_setup"], false, "{v}");
    assert_eq!(v["code_required"], true, "{v}");
    // Le code du journal reste la voie.
    let _ = tune_server::auth::emettre_code_activation(&state);
    let code = tune_server::auth::code_activation_en_cours(&state).unwrap();
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = appel(&state, "127.0.0.1:41234", Method::POST, &[], Some(&corps)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    unsafe {
        std::env::remove_var("TUNE_APPLIANCE");
        std::env::remove_var("TUNE_WEB80_RELAIS");
    }

    // --- Conteneur (pont Docker, Docker Desktop, sans privilèges, Podman) :
    // une connexion relayée par le moteur arrive de la passerelle.
    unsafe {
        std::env::set_var("TUNE_TEST_PASSERELLE_CONTENEUR", "172.17.0.1,fd00::1");
    }
    for passerelle in [
        "172.17.0.1:50000",
        "[::ffff:172.17.0.1]:50000",
        "[fd00::1]:50000",
    ] {
        let state = nouvel_etat();
        assert!(ouvrir_la_premiere_configuration(&state));
        let (_, v) = appel(&state, passerelle, Method::GET, &[], None).await;
        assert_eq!(v["first_setup"], false, "{passerelle} {v}");
        let (st, v) = appel(&state, passerelle, Method::POST, &[], Some(CORPS)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{passerelle} {v}");
        assert!(!active(&state));
    }
    // Un appareil du LAN dont l'adresse est conservée (réseau de l'hôte,
    // redirection qui garde la source) reste accepté.
    let state = nouvel_etat();
    assert!(ouvrir_la_premiere_configuration(&state));
    let (st, v) = appel(&state, "192.168.1.50:50000", Method::POST, &[], Some(CORPS)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Passerelle illisible dans un conteneur : refus.
    unsafe {
        std::env::set_var("TUNE_TEST_PASSERELLE_CONTENEUR", "");
    }
    let state = nouvel_etat();
    assert!(ouvrir_la_premiere_configuration(&state));
    let (st, v) = appel(&state, "192.168.1.50:50000", Method::POST, &[], Some(CORPS)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    unsafe {
        std::env::remove_var("TUNE_TEST_PASSERELLE_CONTENEUR");
    }
}
