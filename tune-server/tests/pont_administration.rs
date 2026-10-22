//! Le pont (accès à distance) se pilote en administrateur.
//!
//! - `POST /cloud/bridge/enable` (sa réponse porte le jeton de pont) et
//!   `POST /cloud/bridge/disable` exigent l'administrateur dès que
//!   l'authentification est active ; sans authentification, rien ne change.
//! - `POST /cloud/bridge/rotate-token` renouvelle le lien d'accès : nouveau
//!   jeton enregistré, ancien remplacé, réponse de la forme de
//!   `GET /cloud/bridge/access-link`, jamais mise en cache ; 409 tant que le
//!   pont est éteint.
//!
//! Ce que le relais fait du nouveau jeton est vérifié dans tune-core
//! (`cloud/relay_renouvellement_tests.rs`) et tune-bridge (`state.rs`).
//!
//! Les valeurs employées ici sont FAUSSES et le restent.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "test-jwt-secret";
const FAUX_JETON: &str = "FAUX-jeton-de-pont-0a1b2c";
const ACTIVER: &str = "/api/v1/cloud/bridge/enable";
const ETEINDRE: &str = "/api/v1/cloud/bridge/disable";
const RENOUVELER: &str = "/api/v1/cloud/bridge/rotate-token";
const LIEN: &str = "/api/v1/cloud/bridge/access-link";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn enable_auth(state: &AppState) {
    let s = reglages(state);
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

fn pont_actif_avec_jeton(state: &AppState) {
    let s = reglages(state);
    s.set("bridge_enabled", "true").unwrap();
    s.set("bridge_token", FAUX_JETON).unwrap();
}

fn tok(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET_JWT).unwrap()
}

fn jeton_enregistre(state: &AppState) -> Option<String> {
    reglages(state).get("bridge_token").unwrap()
}

fn pont_enregistre(state: &AppState) -> Option<String> {
    reglages(state).get("bridge_enabled").unwrap()
}

struct Reponse {
    status: StatusCode,
    corps: String,
    cache: Option<String>,
}

async fn appel(state: &AppState, method: Method, path: &str, bearer: Option<&str>) -> Reponse {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::builder().method(method).uri(path);
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    Reponse {
        status,
        corps: String::from_utf8_lossy(&bytes).to_string(),
        cache,
    }
}

async fn post(state: &AppState, path: &str, bearer: Option<&str>) -> Reponse {
    appel(state, Method::POST, path, bearer).await
}

// ---------------------------------------------------------------------------
// enable / disable : administrateur
// ---------------------------------------------------------------------------

#[tokio::test]
async fn activer_le_pont_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    state.license.set_account_premium(true, None).await;

    let r = post(&state, ACTIVER, None).await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "sans session : {}",
        r.corps
    );
    let r = post(&state, ACTIVER, Some(&tok("user", 2))).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "compte standard : {}",
        r.corps
    );
    assert_eq!(
        pont_enregistre(&state),
        None,
        "un refus a quand même activé le pont"
    );
    assert_eq!(jeton_enregistre(&state), None, "un refus a écrit un jeton");

    let r = post(&state, ACTIVER, Some(&tok("admin", 1))).await;
    assert_eq!(r.status, StatusCode::OK, "administrateur : {}", r.corps);
    let v: Value = serde_json::from_str(&r.corps).unwrap();
    assert_eq!(
        v["bridge_token"].as_str(),
        jeton_enregistre(&state).as_deref()
    );
}

#[tokio::test]
async fn eteindre_le_pont_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);

    let r = post(&state, ETEINDRE, None).await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "sans session : {}",
        r.corps
    );
    let r = post(&state, ETEINDRE, Some(&tok("user", 2))).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "compte standard : {}",
        r.corps
    );
    assert_eq!(
        pont_enregistre(&state).as_deref(),
        Some("true"),
        "un refus a quand même éteint le pont"
    );

    let r = post(&state, ETEINDRE, Some(&tok("admin", 1))).await;
    assert_eq!(r.status, StatusCode::OK, "administrateur : {}", r.corps);
    assert_eq!(pont_enregistre(&state).as_deref(), Some("false"));
}

#[tokio::test]
async fn sans_authentification_activer_et_eteindre_restent_ouverts() {
    // Le cas par défaut (`auth_enabled` absent) : rien ne change.
    let state = new_state();
    state.license.set_account_premium(true, None).await;

    let r = post(&state, ACTIVER, None).await;
    assert_eq!(r.status, StatusCode::OK, "activer : {}", r.corps);
    assert_eq!(pont_enregistre(&state).as_deref(), Some("true"));
    let r = post(&state, ETEINDRE, None).await;
    assert_eq!(r.status, StatusCode::OK, "éteindre : {}", r.corps);
    assert_eq!(pont_enregistre(&state).as_deref(), Some("false"));
}

// ---------------------------------------------------------------------------
// rotate-token : renouveler le lien d'accès
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renouveler_rend_un_nouveau_lien_et_remplace_le_jeton() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);
    let admin = tok("admin", 1);

    let r = post(&state, RENOUVELER, Some(&admin)).await;
    assert_eq!(r.status, StatusCode::OK, "administrateur : {}", r.corps);
    assert_eq!(
        r.cache.as_deref(),
        Some("no-store"),
        "un lien porteur de jeton ne se met pas en cache"
    );
    let v: Value = serde_json::from_str(&r.corps).unwrap();
    let nouveau = jeton_enregistre(&state).expect("plus de jeton enregistré");
    assert_ne!(nouveau, FAUX_JETON, "le jeton n'a pas changé");
    assert!(
        uuid::Uuid::parse_str(&nouveau).is_ok_and(|u| u.get_version_num() == 4),
        "le nouveau jeton doit être un UUID v4"
    );
    assert!(
        !r.corps.contains(FAUX_JETON),
        "la réponse porte l'ancien jeton"
    );

    let sid = v["server_id"].as_str().expect("server_id absent");
    let attendu = format!("https://bridge.mozaiklabs.fr/{sid}/#token={nouveau}");
    assert_eq!(v["link"].as_str(), Some(attendu.as_str()), "lien inattendu");
    assert_eq!(
        v["access_url"].as_str(),
        Some(format!("https://bridge.mozaiklabs.fr/{sid}/").as_str())
    );

    // Le lien servi ensuite est le nouveau, à l'identique.
    let lien = appel(&state, Method::GET, LIEN, Some(&admin)).await;
    let l: Value = serde_json::from_str(&lien.corps).unwrap();
    assert_eq!(l["link"], v["link"]);

    // Deux renouvellements : deux jetons différents.
    post(&state, RENOUVELER, Some(&admin)).await;
    assert_ne!(jeton_enregistre(&state).as_deref(), Some(nouveau.as_str()));
}

#[tokio::test]
async fn renouveler_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);

    let r = post(&state, RENOUVELER, None).await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "sans session : {}",
        r.corps
    );
    let r = post(&state, RENOUVELER, Some(&tok("user", 2))).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "compte standard : {}",
        r.corps
    );
    assert_eq!(
        jeton_enregistre(&state).as_deref(),
        Some(FAUX_JETON),
        "un refus a quand même changé le jeton"
    );
}

#[tokio::test]
async fn renouveler_pont_eteint_rend_409_sans_rien_ecrire() {
    let state = new_state();
    enable_auth(&state);
    let admin = tok("admin", 1);

    let r = post(&state, RENOUVELER, Some(&admin)).await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "pont jamais activé : {}",
        r.corps
    );
    assert_eq!(jeton_enregistre(&state), None, "un jeton a été écrit");

    pont_actif_avec_jeton(&state);
    reglages(&state).set("bridge_enabled", "false").unwrap();
    let r = post(&state, RENOUVELER, Some(&admin)).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "pont éteint : {}", r.corps);
    assert_eq!(jeton_enregistre(&state).as_deref(), Some(FAUX_JETON));
}

#[tokio::test]
async fn sans_authentification_renouveler_reste_ouvert() {
    let state = new_state();
    pont_actif_avec_jeton(&state);

    let r = post(&state, RENOUVELER, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.corps);
    assert_ne!(jeton_enregistre(&state).as_deref(), Some(FAUX_JETON));
}
