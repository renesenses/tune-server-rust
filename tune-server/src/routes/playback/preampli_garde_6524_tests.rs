use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn la_lecture_du_preampli_sans_sortie_n_invente_pas_de_gain_6524() {
    crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = repo
        .create("Banc sans appareil", Some("local"), None)
        .unwrap();
    let avant = state.playback.get_state(zone).await;
    let reponse = super::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/{zone}/eq/retained-preamp"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(reponse.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body, json!({"db":null,"returning":false}));
    let apres = state.playback.get_state(zone).await;
    assert_eq!(avant.state, apres.state);
    assert_eq!(avant.volume, apres.volume);
}
