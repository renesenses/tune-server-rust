use axum::{body::Body, http::Request};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::poller::ZonePollerMetrics;
use tune_server::state::AppState;

async fn get(state: &AppState, path: &str) -> Value {
    let response = tune_server::routes::router(state.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn les_deux_routes_exposent_le_compteur_mesure_et_sa_remise_a_zero() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Cast", Some("chromecast"), Some("chromecast-test"))
        .unwrap();
    for errors in [79_u32, 300, 0] {
        state.poller_metrics.lock().await.insert(
            id,
            ZonePollerMetrics {
                echecs_sondage_repos: errors,
                total_errors: 7,
                total_polls: 100,
                ..Default::default()
            },
        );
        let health = get(&state, &format!("/api/v1/zones/{id}/network-health")).await;
        assert_eq!(health["echecs_sondage_repos"], errors);
        assert_eq!(
            health["poll_errors"], 7,
            "the existing counter keeps its meaning"
        );
        let report = get(&state, "/api/v1/system/diagnostics").await;
        assert_eq!(
            report["zone_poller_metrics"][id.to_string()]["echecs_sondage_repos"],
            errors
        );
    }
}

#[tokio::test]
async fn une_zone_sans_sondage_ne_se_voit_pas_inventer_une_panne() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("New", Some("chromecast"), Some("chromecast-new"))
        .unwrap();
    let health = get(&state, &format!("/api/v1/zones/{id}/network-health")).await;
    assert_eq!(health["echecs_sondage_repos"], 0);
    let report = get(&state, "/api/v1/system/diagnostics").await;
    assert!(
        report["zone_poller_metrics"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}
