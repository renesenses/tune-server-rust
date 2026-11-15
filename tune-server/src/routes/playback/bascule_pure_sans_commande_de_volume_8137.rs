//! #8137 (fil 1771) — un renderer DLNA en volume fixe coupait son signal à
//! chaque bascule PURE, flux conservé. La bascule ne doit lui envoyer aucune
//! commande de volume : la zone est déjà à 100 %. Une zone à volume variable
//! garde son comportement : 100 % à l'entrée sous verrou, consigne recomposée
//! à la sortie (#6213). Les commandes sont comptées SUR LA SORTIE (factice).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna-renderer-8137";

/// Une zone DLNA à 40 %, trim de −2 dB, PURE éteint, verrou global armé.
async fn banc(volume_fixe: bool) -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = repo.create("Salon", Some("dlna"), Some(APPAREIL)).unwrap();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set(&format!("zone_{zone_id}_gain_trim_db"), "-2")
        .unwrap();
    reglages
        .set(
            &format!("zone_{zone_id}_audiophile"),
            r#"{"enabled":false,"lock_volume":true}"#,
        )
        .unwrap();
    if volume_fixe {
        repo.update_fixed_volume(zone_id, true).unwrap();
        repo.update_volume(zone_id, 100.0).unwrap();
        state.playback.set_volume(zone_id, 1.0).await;
    } else {
        repo.update_volume(zone_id, 40.0).unwrap();
        state.playback.set_volume(zone_id, 0.4).await;
    }
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
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

/// LE défaut du fil 1771 : zone en volume fixe, PURE aller-retour.
#[tokio::test]
async fn volume_fixe_aller_retour_pure_sans_commande_de_volume_8137() {
    let (app, state, zone_id) = banc(true).await;
    assert_eq!(basculer_pure(&app, zone_id, true).await, StatusCode::OK);
    assert_eq!(basculer_pure(&app, zone_id, false).await, StatusCode::OK);
    assert_eq!(basculer_pure(&app, zone_id, true).await, StatusCode::OK);
    assert_eq!(basculer_pure(&app, zone_id, false).await, StatusCode::OK);
    assert_eq!(
        commandes_de_volume(&state).await,
        Vec::<f64>::new(),
        "#8137 : zone en volume fixe, la bascule PURE ne doit envoyer aucune \
         commande de volume à l'appareil"
    );
    assert_eq!(state.playback.get_state(zone_id).await.volume, 1.0);
}

/// Zone à volume variable : 100 % à l'entrée sous verrou, consigne
/// recomposée (trim compris) à la sortie, comme avant (#5695, #6213).
#[tokio::test]
async fn volume_variable_aller_retour_pure_inchange_8137() {
    let (app, state, zone_id) = banc(false).await;
    assert_eq!(basculer_pure(&app, zone_id, true).await, StatusCode::OK);
    assert_eq!(commandes_de_volume(&state).await, vec![1.0]);
    assert_eq!(basculer_pure(&app, zone_id, false).await, StatusCode::OK);
    let recu = commandes_de_volume(&state).await;
    let attendu = 10f64.powf(-2.0 / 20.0);
    assert_eq!(
        recu.len(),
        2,
        "une commande à l'entrée, une à la sortie : {recu:?}"
    );
    assert!(
        (recu[1] - attendu).abs() < 1e-6,
        "#6213 : à la sortie, 100 % × trim −2 dB ({attendu:.4}) ; reçu {recu:?}"
    );
}
