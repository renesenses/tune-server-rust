//! #7726 — retirer la licence d'un serveur Tune doit libérer cette machine
//! auprès de mozaiklabs.fr, pas seulement effacer la clé en local.
//!
//! Avant le correctif, `DELETE /system/license` et `POST
//! /cloud/license/deactivate` effaçaient la clé sans prévenir personne : le
//! site gardait l'ancienne machine comme détentrice de la licence, et
//! l'activation sur un autre serveur était refusée (« License active on
//! another server »).
//!
//! Le serveur de licences est bouchonné en local (`mozaik_base_url`), jamais
//! mozaiklabs.fr. Quand le site est injoignable, la clé doit être effacée
//! quand même et la réponse doit le dire.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "TUNE-LIFETIME-7726-RETRAIT";
const DELETE_SYSTEM: &str = "/api/v1/system/license";
const DEACTIVATE_CLOUD: &str = "/api/v1/cloud/license/deactivate";

/// Corps reçus par le faux site sur sa route de libération de machine.
type Recus = Arc<Mutex<Vec<Value>>>;

/// Faux mozaiklabs.fr, sur une socket locale éphémère. Rend l'URL de base et
/// les corps reçus sur `/api/v1/license/deactivate`.
async fn faux_site_de_licences() -> (String, Recus) {
    let recus: Recus = Arc::new(Mutex::new(Vec::new()));
    let journal = recus.clone();
    let app = axum::Router::new().route(
        "/api/v1/license/deactivate",
        axum::routing::post(move |axum::Json(corps): axum::Json<Value>| {
            let journal = journal.clone();
            async move {
                journal.lock().unwrap().push(corps);
                axum::Json(json!({ "status": "released" }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), recus)
}

/// Une adresse où personne n'écoute : on réserve un port puis on le rend.
async fn site_injoignable() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Un serveur Tune qui détient la clé `CLE`, le site redirigé vers `base_url`.
async fn serveur_avec_licence(
    base_url: &str,
) -> (axum::Router, AppState, tune_core::test_scratch::ScratchDir) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i7726");
    let db = dir.join("library.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base_url).unwrap();
    settings.set("server_id", "serveur-7726").unwrap();
    state.license.set_license_key(CLE).await.unwrap();
    (tune_server::routes::router(state.clone()), state, dir)
}

async fn appeler(app: &axum::Router, methode: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Vérifie que le site a reçu une seule libération, pour cette clé et avec
/// l'empreinte de CETTE machine.
async fn assert_machine_liberee(recus: &Recus, state: &AppState) {
    let empreinte = state.license.license_state().await.hardware_fingerprint;
    assert!(!empreinte.is_empty(), "empreinte de la machine vide");
    let recus = recus.lock().unwrap();
    assert_eq!(
        recus.len(),
        1,
        "aucun appel de libération de la machine reçu par le site : le retrait \
         n'a fait qu'effacer la clé en local ({recus:?})"
    );
    assert_eq!(recus[0]["license_key"], CLE, "corps reçu : {}", recus[0]);
    assert_eq!(
        recus[0]["hardware_fingerprint"], empreinte,
        "la libération doit porter l'empreinte de la machine : {}",
        recus[0]
    );
    assert_eq!(
        recus[0]["server_id"], "serveur-7726",
        "corps reçu : {}",
        recus[0]
    );
}

async fn assert_cle_effacee(state: &AppState) {
    let ls = state.license.license_state().await;
    assert_eq!(ls.license_key, None, "la clé locale n'est pas effacée");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(settings.get("license_key").ok().flatten(), None);
}

#[tokio::test]
async fn retirer_la_licence_libere_la_machine_aupres_du_site() {
    let (base, recus) = faux_site_de_licences().await;
    let (app, state, _dir) = serveur_avec_licence(&base).await;

    let (status, body) = appeler(&app, "DELETE", DELETE_SYSTEM).await;

    assert_eq!(status, StatusCode::OK, "réponse : {body}");
    assert_machine_liberee(&recus, &state).await;
    assert_cle_effacee(&state).await;
    assert_eq!(body["status"], "ok", "réponse : {body}");
    assert_eq!(body["tier"], "free", "réponse : {body}");
    assert_eq!(body["remote_released"], true, "réponse : {body}");
    assert!(body.get("message_key").is_none(), "réponse : {body}");
}

#[tokio::test]
async fn desactiver_cette_machine_libere_aussi_la_machine() {
    let (base, recus) = faux_site_de_licences().await;
    let (app, state, _dir) = serveur_avec_licence(&base).await;

    let (status, body) = appeler(&app, "POST", DEACTIVATE_CLOUD).await;

    assert_eq!(status, StatusCode::OK, "réponse : {body}");
    assert_machine_liberee(&recus, &state).await;
    assert_cle_effacee(&state).await;
    assert_eq!(body["status"], "deactivated", "réponse : {body}");
    assert_eq!(body["remote_released"], true, "réponse : {body}");
}

#[tokio::test]
async fn site_injoignable_la_cle_est_effacee_et_la_reponse_le_dit() {
    let base = site_injoignable().await;
    let (app, state, _dir) = serveur_avec_licence(&base).await;

    let (status, body) = appeler(&app, "DELETE", DELETE_SYSTEM).await;

    assert_eq!(status, StatusCode::OK, "réponse : {body}");
    assert_cle_effacee(&state).await;
    assert_eq!(body["tier"], "free", "réponse : {body}");
    assert_eq!(
        body["remote_released"], false,
        "la réponse doit dire que le site n'a pas été prévenu : {body}"
    );
    assert_eq!(
        body["message_key"], "api.license.removed_offline",
        "réponse : {body}"
    );
}
