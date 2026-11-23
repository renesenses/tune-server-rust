//! Session « rester connecté » des remotes : `POST /auth/login` avec
//! `"remember": true` rend un jeton de 30 jours, listé et révocable par
//! l'administrateur ; sans ce champ, rien ne change (24 h).

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use std::net::SocketAddr;
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const DISTANT: &str = "192.168.1.60:50000";
const CETTE_MACHINE: &str = "127.0.0.1:50000";
const SECRET: &str = "secret-du-banc-session-longue";
const TRENTE_JOURS: u64 = 30 * 86400;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Authentification active, un admin (`proprio`) et un utilisateur (`alice`).
fn preparer() -> AppState {
    let state = new_state();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", SECRET).unwrap();
    for (nom, admin) in [("proprio", true), ("alice", false)] {
        let h = tune_server::auth::hash_password(&format!("{nom}-mdp-1234")).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO profiles (username, display_name, password_hash_v2, is_admin) \
                 VALUES (?, ?, ?, ?)",
                &[
                    &nom as &dyn ToSqlValue,
                    &nom as &dyn ToSqlValue,
                    &h as &dyn ToSqlValue,
                    &admin as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }
    state
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

async fn connexion(state: &AppState, nom: &str, extra: &str) -> Value {
    let corps = format!(r#"{{"username":"{nom}","password":"{nom}-mdp-1234"{extra}}}"#);
    let (st, v) = appel(
        state,
        DISTANT,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(&corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn duree_du_jeton(jeton: &str) -> u64 {
    let c = tune_server::auth::verify_jwt(jeton, SECRET).unwrap();
    c.exp - c.iat
}

async fn moi(state: &AppState, jeton: &str) -> StatusCode {
    appel(
        state,
        DISTANT,
        Method::GET,
        "/api/v1/auth/me",
        Some(jeton),
        None,
    )
    .await
    .0
}

#[tokio::test]
async fn remember_donne_trente_jours_et_sans_lui_rien_ne_change() {
    let state = preparer();

    let v = connexion(&state, "alice", r#","remember":true"#).await;
    assert_eq!(v["expires_in"], TRENTE_JOURS, "{v}");
    let long = v["token"].as_str().unwrap();
    assert_eq!(duree_du_jeton(long), TRENTE_JOURS);
    assert_eq!(v["role"], "user", "aucun élargissement de rôle");
    assert_eq!(moi(&state, long).await, StatusCode::OK);

    for extra in ["", r#","remember":false"#] {
        let v = connexion(&state, "alice", extra).await;
        assert_eq!(v["expires_in"], 86400, "{extra} : {v}");
        assert_eq!(duree_du_jeton(v["token"].as_str().unwrap()), 86400);
    }
}

#[tokio::test]
async fn l_admin_liste_et_revoque_la_session_longue() {
    let state = preparer();
    let admin = connexion(&state, "proprio", "").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let long = connexion(&state, "alice", r#","remember":true"#).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(moi(&state, &long).await, StatusCode::OK);

    // Un utilisateur ordinaire ne liste pas les sessions.
    let (st, _) = appel(
        &state,
        DISTANT,
        Method::GET,
        "/api/v1/auth/sessions",
        Some(&long),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let (st, v) = appel(
        &state,
        DISTANT,
        Method::GET,
        "/api/v1/auth/sessions",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let sessions = v["sessions"].as_array().expect("liste des sessions");
    assert_eq!(sessions.len(), 1, "{v}");
    assert_eq!(sessions[0]["username"], "alice");
    let id = sessions[0]["id"].as_str().unwrap().to_string();

    let (st, v) = appel(
        &state,
        DISTANT,
        Method::DELETE,
        &format!("/api/v1/auth/sessions/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    assert_eq!(moi(&state, &long).await, StatusCode::UNAUTHORIZED);
    let (st, _) = appel(
        &state,
        DISTANT,
        Method::GET,
        "/api/v1/zones",
        Some(&long),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "jeton révoqué refusé");
    // Le jeton ordinaire de l'admin n'est pas touché.
    assert_eq!(moi(&state, &admin).await, StatusCode::OK);
}

#[tokio::test]
async fn couper_puis_reactiver_l_authentification_invalide_la_session_longue() {
    let state = preparer();
    let admin = connexion(&state, "proprio", "").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let long = connexion(&state, "alice", r#","remember":true"#).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, v) = appel(
        &state,
        DISTANT,
        Method::POST,
        "/api/v1/auth/config",
        Some(&admin),
        Some(r#"{"auth_enabled":false}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/config",
        None,
        Some(r#"{"auth_enabled":true}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    assert_eq!(moi(&state, &long).await, StatusCode::UNAUTHORIZED);
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert!(
        reglages
            .par_prefixe("auth_session_token:")
            .unwrap()
            .is_empty(),
        "plus aucune session longue inscrite"
    );
}

#[tokio::test]
async fn changer_le_mot_de_passe_invalide_la_session_longue() {
    let state = preparer();
    let long = connexion(&state, "alice", r#","remember":true"#).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(moi(&state, &long).await, StatusCode::OK);

    let h = tune_server::auth::hash_password("nouveau-mdp-5678").unwrap();
    state
        .backend
        .execute(
            "UPDATE profiles SET password_hash_v2 = ? WHERE username = 'alice'",
            &[&h as &dyn ToSqlValue],
        )
        .unwrap();
    assert_eq!(moi(&state, &long).await, StatusCode::UNAUTHORIZED);
}
