//! #6213 (GgB, fil 1797) — trim de zone à −2 dB, volume à 100 %, PURE
//! verrouillé puis relâché : l'appareil gardait les 100 % sans trim que
//! l'entrée en PURE avait commandés, jusqu'au prochain geste sur le curseur.
//! Les commandes sont comptées SUR LA SORTIE (factice).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "local:dac-z8";

async fn banc() -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = repo
        .create("DAC-Z8", Some("local"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 100.0).unwrap();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set(&format!("zone_{zone_id}_gain_trim_db"), "-2")
        .unwrap();
    reglages
        .set(
            &format!("zone_{zone_id}_audiophile"),
            r#"{"enabled":true,"lock_volume":true}"#,
        )
        .unwrap();
    state.playback.set_volume(zone_id, 1.0).await;
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "DAC-Z8").with_type("local"),
    ));
    (crate::routes::router(state.clone()), state, zone_id)
}

async fn commandes_de_volume(state: &crate::state::AppState) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let sortie = outputs.get(APPAREIL).expect("sortie enregistrée");
    let sortie = sortie.lock().await;
    sortie
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

async fn basculer_pure(app: &axum::Router, zone_id: i64, actif: bool) -> StatusCode {
    let requete = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/zones/{zone_id}/audiophile"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({ "enabled": actif, "confirm_full_volume": true }).to_string(),
        ))
        .unwrap();
    app.clone().oneshot(requete).await.unwrap().status()
}

#[tokio::test]
async fn sortir_de_pure_renvoie_la_consigne_avec_le_trim_6213() {
    let (app, state, zone_id) = banc().await;

    assert_eq!(basculer_pure(&app, zone_id, false).await, StatusCode::OK);

    let attendu = 10f64.powf(-2.0 / 20.0);
    let recu = commandes_de_volume(&state).await;
    let dernier = recu.last().copied();
    assert!(
        dernier.is_some_and(|v| (v - attendu).abs() < 1e-6),
        "#6213 : après PURE off, l'appareil doit recevoir 100 % × trim −2 dB \
         ({attendu:.4}) sans qu'on touche au curseur ; commandes reçues : {recu:?}"
    );
}

/// Une requête qui ne quitte pas le verrou ne commande rien de plus.
#[tokio::test]
async fn rester_en_pure_ne_commande_aucun_volume() {
    let (app, state, zone_id) = banc().await;
    assert_eq!(basculer_pure(&app, zone_id, true).await, StatusCode::OK);
    assert_eq!(commandes_de_volume(&state).await, Vec::<f64>::new());
}
