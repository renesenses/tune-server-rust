//! #8165 — de bout en bout par le routeur public : un geste de volume sur une
//! zone en sourdine lève la sourdine ; un PATCH qui dit la sourdine la garde.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna-8165";

async fn serveur() -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = repo
        .create("Salon 8165", Some("dlna"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 40.0).unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon 8165").with_type("dlna"),
    ));
    state.playback.set_volume(zone_id, 0.40).await;
    state
        .orchestrator
        .set_mute(zone_id, true, Some(APPAREIL))
        .await
        .unwrap();
    (routeur, state, zone_id)
}

async fn appeler(app: &axum::Router, methode: &str, chemin: &str, corps: Value) -> StatusCode {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    app.clone().oneshot(requete).await.unwrap().status()
}

fn muette(state: &crate::state::AppState, zone_id: i64) -> bool {
    ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .muted
}

#[tokio::test]
async fn le_curseur_de_volume_leve_la_sourdine_8165() {
    let (app, state, zone_id) = serveur().await;
    let statut = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{zone_id}/volume"),
        json!({"volume": 20}),
    )
    .await;
    assert!(statut.is_success(), "{statut}");
    assert!(
        !muette(&state, zone_id),
        "#8165 : baisser le volume d'une zone en sourdine lève la sourdine"
    );
    assert!(!state.playback.get_state(zone_id).await.muted);
}

#[tokio::test]
async fn le_volume_de_lecture_leve_la_sourdine_8165() {
    let (app, state, zone_id) = serveur().await;
    let statut = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone_id}/volume"),
        json!({"volume": 0.2}),
    )
    .await;
    assert!(statut.is_success(), "{statut}");
    assert!(
        !muette(&state, zone_id),
        "#8165 : POST /zones/{{id}}/volume (route de lecture)"
    );
}

#[tokio::test]
async fn un_patch_qui_dit_la_sourdine_la_garde_8165() {
    let (app, state, zone_id) = serveur().await;
    let statut = appeler(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"volume": 20, "muted": true}),
    )
    .await;
    assert!(statut.is_success(), "{statut}");
    assert!(
        muette(&state, zone_id),
        "le PATCH qui pose aussi `muted: true` garde la sourdine"
    );
}
