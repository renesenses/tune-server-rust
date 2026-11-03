//! Vraie route /ai/query, repli local sans clé IA et sortie factice DLNA.
use super::*;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;
use tune_core::{db::zone_repo::ZoneRepo, outputs::mock::MockOutput};

async fn requete(initial: f64, message: &str) -> (Value, Vec<f64>, f64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    assert!(
        Interlocuteur::choisir(&state, &s).is_none(),
        "le banc ne doit contacter aucune IA"
    );
    let id = ZoneRepo::with_backend(state.backend.clone())
        .create("Banc assistant 6836", Some("dlna"), Some("dlna-6836-http"))
        .unwrap();
    state.playback.set_volume(id, initial).await;
    for cle in ["volume_limit_safety", "volume_limit_comfort"] {
        s.set(&format!("zone_{id}_{cle}"), "1.0").unwrap();
    }
    state.outputs.lock().await.register(Box::new(
        MockOutput::new("dlna-6836-http", "Banc").with_type("dlna"),
    ));
    let app = Router::new()
        .nest("/api/v1/ai", router())
        .with_state(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ai/query")
                .header("Content-Type", "application/json")
                .header("Accept-Language", "fr")
                .body(Body::from(
                    json!({"message":message,"zone_id":id}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap();
    let out = state.outputs.lock().await.get("dlna-6836-http").unwrap();
    let guard = out.lock().await;
    let appels = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .volume_calls()
        .await;
    let final_volume = state.playback.get_state(id).await.volume;
    (body, appels, final_volume)
}

#[tokio::test]
async fn la_route_refuse_le_plein_volume_arrondi_sans_commande_6836() {
    for message in ["volume 99.5", "volume 99.9", "volume 100"] {
        let (r, appels, volume) = requete(0.97, message).await;
        assert_eq!(
            r["actions"][0]["result"]["error"], "full_volume_refused",
            "#6836 HTTP : {message} a accepté le plein volume : {r}"
        );
        assert!(
            appels.is_empty(),
            "#6836 HTTP : commande envoyée {appels:?}"
        );
        assert_eq!(volume, 0.97);
        assert_eq!(r["reply"], crate::i18n::t("fr", "ai.volumeRefused"));
    }
}
#[tokio::test]
async fn la_route_refuse_le_saut_et_accepte_un_pas_ou_une_baisse_6836() {
    for message in ["volume 24", "volume 100"] {
        let (r, appels, volume) = requete(0.20, message).await;
        assert_eq!(
            r["actions"][0]["result"]["error"], "volume_raise_too_large",
            "{r}"
        );
        assert!(appels.is_empty());
        assert_eq!(volume, 0.20);
    }
    for (message, attendu) in [("volume 23", 0.23), ("volume 0", 0.0)] {
        let (r, appels, volume) = requete(0.20, message).await;
        assert_eq!(r["actions"][0]["result"]["status"], "volume_set", "{r}");
        assert_eq!(appels, vec![attendu]);
        assert_eq!(volume, attendu);
    }
}
#[tokio::test]
async fn la_route_sans_nombre_ne_pose_pas_un_volume_par_defaut_6836() {
    let (r, appels, volume) = requete(0.48, "volume").await;
    assert!(
        appels.is_empty(),
        "#6836 HTTP : valeur inventée {appels:?}, {r}"
    );
    assert_eq!(volume, 0.48);
    assert!(
        r["actions"]
            .as_array()
            .is_none_or(|a| a.iter().all(|x| x["tool"] != "set_volume"))
    );
}
