//! #5189 — `GET /system/diagnostics` publie `cpu_temp_c`.
//!
//! L'écran « État du serveur » du client web lit ce champ : un nombre en °C,
//! ou `null` sans capteur (macOS, Windows, conteneur, machine virtuelle). Il ne
//! doit JAMAIS être absent : le client distingue « serveur trop ancien » (champ
//! absent) de « pas de capteur » (`null`). Le choix du capteur lui-même est
//! éprouvé sur une arborescence sysfs simulée dans
//! `tune-core/src/audio/thermal.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn le_rapport_de_diagnostic_publie_la_temperature_du_processeur() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::get("/api/v1/system/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let diag: Value = serde_json::from_slice(&bytes).unwrap();

    let temp = diag
        .get("cpu_temp_c")
        .unwrap_or_else(|| panic!("cpu_temp_c absent du rapport : {diag}"));
    match temp {
        Value::Null => {}
        Value::Number(n) => {
            let c = n.as_f64().unwrap();
            assert!(
                (5.0..=125.0).contains(&c),
                "température hors du domaine physique : {c}"
            );
        }
        autre => panic!("cpu_temp_c doit être un nombre ou null, pas {autre}"),
    }

    // Hors Linux, aucune lecture : toujours `null`, jamais un 0 °C inventé.
    #[cfg(not(target_os = "linux"))]
    assert!(temp.is_null(), "hors Linux, cpu_temp_c doit valoir null");
}
