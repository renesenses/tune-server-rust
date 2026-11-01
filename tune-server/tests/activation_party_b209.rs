//! rc11 (Bertrand, 01/11/2026) — Mode Party, greffon WASM livré avec Tune
//! (0.9.23) et actif d'office, se désactive et se réactive depuis Réglages ›
//! Extensions par `POST /plugins/party/{disable,enable}`. Actif par défaut :
//! rien ne change pour qui ne touche à rien.
//!
//! #7428 — il n'apparaît qu'UNE fois dans la liste, avec son état.
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

use super::plugin_wasm_restart_required_5112::{demarrer, post};

async fn cartes_party(app: &axum::Router) -> Vec<Value> {
    let resp = app
        .clone()
        .oneshot(Request::get("/api/v1/plugins").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let liste: Value = serde_json::from_slice(&octets).unwrap();
    liste
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "party")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actif_par_defaut_une_seule_carte() {
    let _e = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let (state, app, _env) = demarrer(dossier.path(), true).await;
    // Contre-épreuve de « actif d'office » : on EFFACE le réglage, comme une
    // base qui n'y a jamais touché.
    SettingsRepo::with_backend(state.backend.clone())
        .delete("plugin_party_enabled")
        .unwrap();
    let cartes = cartes_party(&app).await;
    assert_eq!(cartes.len(), 1, "une seule carte Party : {cartes:?}");
    assert_eq!(cartes[0]["enabled"], true, "{}", cartes[0]);
    assert_eq!(cartes[0]["loaded"], true, "{}", cartes[0]);
    assert_eq!(cartes[0]["restart_required"], false, "{}", cartes[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decocher_annonce_le_redemarrage_et_recocher_l_annule() {
    let _e = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let (state, app, _env) = demarrer(dossier.path(), true).await;
    let reglages = SettingsRepo::with_backend(state.backend.clone());

    post(&app, "/api/v1/plugins/party/disable").await;
    assert_eq!(
        reglages.get("plugin_party_enabled").unwrap().as_deref(),
        Some("false")
    );
    let c = cartes_party(&app).await;
    assert_eq!(c.len(), 1);
    assert_eq!(c[0]["enabled"], false, "{}", c[0]);
    assert_eq!(
        c[0]["restart_required"], true,
        "il tourne encore : le décharger attend un redémarrage — {}",
        c[0]
    );

    post(&app, "/api/v1/plugins/party/enable").await;
    let c = cartes_party(&app).await;
    assert_eq!(c[0]["enabled"], true, "{}", c[0]);
    assert_eq!(c[0]["restart_required"], false, "{}", c[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decoche_au_demarrage_il_n_est_pas_charge_et_se_rearme() {
    let _e = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    // `demarrer(.., false)` vérifie lui-même que Party n'est PAS chargé.
    let (_state, app, _env) = demarrer(dossier.path(), false).await;
    let c = cartes_party(&app).await;
    assert_eq!(
        c.len(),
        1,
        "toujours listé pour pouvoir être recoché : {c:?}"
    );
    assert_eq!(c[0]["enabled"], false, "{}", c[0]);
    assert_eq!(c[0]["loaded"], false, "{}", c[0]);
    assert_eq!(c[0]["restart_required"], false, "{}", c[0]);

    // Ses routes ne sont pas servies tant qu'il n'est pas chargé.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/plugins/party/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        axum::http::StatusCode::OK,
        "Party décoché ne répond pas"
    );

    post(&app, "/api/v1/plugins/party/enable").await;
    let c = cartes_party(&app).await;
    assert_eq!(c[0]["enabled"], true, "{}", c[0]);
    assert_eq!(c[0]["restart_required"], true, "{}", c[0]);
}
