//! Activation de l'authentification depuis l'interface (`/auth/activation`).
//!
//! Tant que l'authentification est coupée, celui qui l'active choisit le mot
//! de passe du compte administrateur : seulement depuis cette machine, ou avec
//! le code à usage unique écrit dans le journal du serveur. Une fois active,
//! l'écran ne recrée plus d'administrateur.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CETTE_MACHINE: &str = "127.0.0.1:50000";
const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

/// Le mot de passe enregistré du compte administrateur (profil 1).
fn mot_de_passe_admin(state: &AppState) -> Option<String> {
    state
        .backend
        .query_one("SELECT password_hash_v2 FROM profiles WHERE id = 1", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .filter(|h| !h.is_empty())
}

async fn appel(
    state: &AppState,
    adresse: &str,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).to_string()));
    (status, v)
}

async fn activer(state: &AppState, adresse: &str, corps: &str) -> (StatusCode, Value) {
    appel(
        state,
        adresse,
        Method::POST,
        "/api/v1/auth/activation",
        None,
        Some(corps),
    )
    .await
}

#[tokio::test]
async fn l_etat_dit_si_un_code_est_necessaire() {
    let state = new_state();
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, "/api/v1/auth/activation", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], false);
    assert_eq!(v["from_this_machine"], false);
    assert_eq!(v["code_required"], true);
    assert_eq!(v["admin_username"], "default");
    let (_, v) = appel(&state, CETTE_MACHINE, Method::GET, "/api/v1/auth/activation", None, None).await;
    assert_eq!(v["from_this_machine"], true);
    assert_eq!(v["code_required"], false);
}

#[tokio::test]
async fn un_tiers_du_lan_sans_code_est_refuse() {
    let state = new_state();
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, r#"{"password":"pirate-1234"}"#).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_required", "{v}");
    assert!(!active(&state));
    assert_eq!(mot_de_passe_admin(&state), None);
    // Relayé (vu de 127.0.0.1, émis ailleurs) : pas « cette machine ».
    let app = tune_server::routes::router(state.clone())
        .layer(MockConnectInfo(CETTE_MACHINE.parse::<SocketAddr>().unwrap()));
    let req = Request::post("/api/v1/auth/activation")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-tune-relais", "1")
        .body(Body::from(r#"{"password":"pirate-1234"}"#))
        .unwrap();
    let st = app.oneshot(req).await.unwrap().status();
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(!active(&state));
}

#[tokio::test]
async fn un_code_faux_est_refuse_et_cinq_essais_detruisent_le_code() {
    let state = new_state();
    // La route écrit le code dans le journal et ne le rend pas.
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::POST, "/api/v1/auth/activation/code", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v.get("code").is_none(), "le code ne sort jamais par l'API : {v}");

    // Ce que l'exploitant lit dans le journal.
    let code = tune_server::auth::emettre_code_activation(&state);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, r#"{"password":"pirate-1234","code":"AAAAAAAAAA"}"#).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_invalid", "{v}");
    assert!(!active(&state));
    for _ in 1..tune_server::auth::ESSAIS_CODE_ACTIVATION {
        activer(&state, AUTRE_HOTE_DU_LAN, r#"{"password":"pirate-1234","code":"BBBBBBBBBB"}"#).await;
    }
    // Le code juste ne vaut plus rien après cinq essais faux.
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
    assert_eq!(mot_de_passe_admin(&state), None);
}

#[tokio::test]
async fn un_mot_de_passe_trop_court_ne_consomme_pas_le_code() {
    let state = new_state();
    let code = tune_server::auth::emettre_code_activation(&state);
    let corps = format!(r#"{{"password":"court","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let corps = format!(r#"{{"password":"proprio-1234","code":"{}"}}"#, code.to_lowercase());
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn le_code_juste_active_et_ne_sert_qu_une_fois() {
    let state = new_state();
    let code = tune_server::auth::emettre_code_activation(&state);
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], true);
    assert_eq!(v["role"], "admin");
    assert!(active(&state));
    let jeton = v["token"].as_str().unwrap().to_string();

    // Le jeton rendu est celui d'un administrateur.
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, "/api/v1/auth/me", Some(&jeton), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Le mot de passe choisi ouvre la session administrateur.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"proprio-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["role"], "admin");

    // Une fois active, l'écran ne recrée plus d'administrateur : sans jeton
    // 401, avec le jeton administrateur 409, même code, même machine.
    let (st, _) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = activer(&state, CETTE_MACHINE, r#"{"password":"autre-1234"}"#).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/activation",
        Some(&jeton),
        Some(r#"{"password":"autre-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "auth_already_enabled");
    let (st, _) = appel(&state, CETTE_MACHINE, Method::POST, "/api/v1/auth/activation/code", Some(&jeton), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    // L'état reste lisible sans jeton.
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, "/api/v1/auth/activation", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], true);
    assert!(v["admin_username"].is_null());
}

#[tokio::test]
async fn depuis_cette_machine_aucun_code_n_est_demande() {
    for adresse in [CETTE_MACHINE, "[::1]:50000"] {
        let state = new_state();
        let (st, v) = activer(&state, adresse, r#"{"password":"proprio-1234"}"#).await;
        assert_eq!(st, StatusCode::OK, "{adresse} : {v}");
        assert!(active(&state));
        assert!(mot_de_passe_admin(&state).is_some());
    }
}

#[tokio::test]
async fn les_jetons_emis_avant_l_activation_ne_valent_plus_rien() {
    let state = new_state();
    // Tout est ouvert : un tiers du LAN se fabrique un jeton et une clé.
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::POST, "/api/v1/auth/token", None, Some("{}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let jeton_ancien = v["token"].as_str().unwrap().to_string();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"pirate-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let session_ancienne = v["token"].as_str().unwrap().to_string();
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::POST, "/api/v1/auth/api-key", None, Some("{}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cle = v["key"].as_str().unwrap().to_string();

    let (st, v) = activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(st, StatusCode::OK, "{v}");

    for j in [&jeton_ancien, &session_ancienne] {
        let (st, _) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, "/api/v1/zones", Some(j), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let app = tune_server::routes::router(state.clone())
        .layer(MockConnectInfo(AUTRE_HOTE_DU_LAN.parse::<SocketAddr>().unwrap()));
    let req = Request::get("/api/v1/zones")
        .header(header::AUTHORIZATION, format!("ApiKey {cle}"))
        .header("x-api-key", cle.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone()).get("api_key").unwrap(),
        None
    );
}

#[tokio::test]
async fn le_profil_default_n_est_pas_reclame_depuis_le_lan() {
    let state = new_state();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"pirate-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(mot_de_passe_admin(&state), None, "le mot de passe du tiers n'est pas enregistré");
}

#[tokio::test]
async fn auth_config_n_active_que_depuis_cette_machine() {
    let state = new_state();
    for corps in [r#"{"auth_enabled":true}"#, r#"{"jwt_secret":"secret-du-tiers"}"#] {
        let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::POST, "/api/v1/auth/config", None, Some(corps)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{corps} : {v}");
        assert!(!active(&state));
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert_ne!(
        reglages.get("jwt_secret").unwrap().as_deref(),
        Some("secret-du-tiers")
    );
    let (st, v) = appel(&state, CETTE_MACHINE, Method::POST, "/api/v1/auth/config", None, Some(r#"{"auth_enabled":true}"#)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}
