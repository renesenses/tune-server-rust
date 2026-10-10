//! Le lien d'accès à distance, servi au seul propriétaire.
//!
//! Le client web lit le jeton du pont dans le fragment de l'adresse
//! (`https://bridge.mozaiklabs.fr/{server_id}/#token=…`). Aucun écran ne
//! pouvait pourtant donner ce lien : `GET /cloud/bridge/status` ne rend que
//! `has_token`, à raison, puisqu'il est lisible par tout compte. Seul
//! `POST /cloud/bridge/enable` rendait le jeton, et ce POST réécrit un réglage.
//!
//! `GET /cloud/bridge/access-link` comble ce manque :
//! - administrateur seulement quand l'authentification est active ;
//! - 409 tant que le pont n'est pas activé, ou sans jeton, et le GET n'en
//!   fabrique jamais un ;
//! - le statut public, lui, ne publie toujours pas le jeton.
//!
//! La valeur employée ici est FAUSSE et le reste.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "test-jwt-secret";
const FAUX_JETON: &str = "FAUX-jeton-de-pont-0a1b2c";
const ROUTE: &str = "/api/v1/cloud/bridge/access-link";

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

async fn get(
    state: &AppState,
    path: &str,
    bearer: Option<&str>,
) -> (StatusCode, String, Option<String>) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::get(path);
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
    (status, String::from_utf8_lossy(&bytes).to_string(), cache)
}

#[tokio::test]
async fn l_administrateur_recoit_le_lien_complet() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);

    let (status, corps, cache) = get(&state, ROUTE, Some(&tok("admin", 1))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "l'administrateur doit recevoir le lien : {corps}"
    );
    let v: Value = serde_json::from_str(&corps).unwrap();
    let sid = v["server_id"].as_str().expect("server_id absent");
    let attendu = format!("https://bridge.mozaiklabs.fr/{sid}/#token={FAUX_JETON}");
    assert_eq!(v["link"].as_str(), Some(attendu.as_str()), "lien inattendu");
    assert_eq!(
        v["access_url"].as_str(),
        Some(format!("https://bridge.mozaiklabs.fr/{sid}/").as_str())
    );
    assert_eq!(
        cache.as_deref(),
        Some("no-store"),
        "un lien porteur de jeton ne se met pas en cache"
    );
}

#[tokio::test]
async fn un_compte_standard_ou_anonyme_est_refuse() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);

    let (status, corps, _) = get(&state, ROUTE, Some(&tok("user", 2))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "un compte standard ne doit pas lire le jeton"
    );
    assert!(!corps.contains(FAUX_JETON));

    let (status, corps, _) = get(&state, ROUTE, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "sans jeton de session : 401"
    );
    assert!(!corps.contains(FAUX_JETON));
}

#[tokio::test]
async fn pont_desactive_ou_sans_jeton_rend_409_sans_rien_creer() {
    let state = new_state();
    enable_auth(&state);
    let admin = tok("admin", 1);

    // Pont jamais activé.
    let (status, _, _) = get(&state, ROUTE, Some(&admin)).await;
    assert_eq!(status, StatusCode::CONFLICT, "pont désactivé : 409");

    // Pont activé, mais aucun jeton : le GET ne doit pas en fabriquer un.
    reglages(&state).set("bridge_enabled", "true").unwrap();
    let (status, _, _) = get(&state, ROUTE, Some(&admin)).await;
    assert_eq!(status, StatusCode::CONFLICT, "pas de jeton : 409");
    assert!(
        reglages(&state).get("bridge_token").unwrap().is_none(),
        "un GET a écrit un jeton de pont"
    );
}

#[tokio::test]
async fn le_statut_public_ne_publie_toujours_pas_le_jeton() {
    let state = new_state();
    enable_auth(&state);
    pont_actif_avec_jeton(&state);

    let (_, corps, _) = get(&state, "/api/v1/cloud/bridge/status", Some(&tok("user", 2))).await;
    assert!(
        !corps.contains(FAUX_JETON),
        "le statut laisse sortir le jeton"
    );
}
