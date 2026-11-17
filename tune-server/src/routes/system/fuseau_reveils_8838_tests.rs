use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

async fn request(state: &AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{method} {path}: {status}, body={bytes:?}: {e}")),
    )
}

#[tokio::test]
async fn le_fuseau_invalide_ne_remplace_pas_le_reglage_8838() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let path = "/api/v1/system/config";
    let (status, _) = request(
        &state,
        "PATCH",
        path,
        json!({"alarms_timezone":"Europe/Paris"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for invalid in [json!("Europe/Inconnue"), json!(42)] {
        let (status, _) = request(&state, "PATCH", path, json!({"alarms_timezone":invalid})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            SettingsRepo::with_backend(state.backend.clone())
                .get("alarms_timezone")
                .unwrap()
                .as_deref(),
            Some("Europe/Paris")
        );
    }
    let (status, config) = request(&state, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["alarms_timezone"], "Europe/Paris");
}

#[tokio::test]
async fn la_liste_de_reveils_annonce_fuseau_et_prochaine_occurrence_8838() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("alarms_timezone", "Europe/Paris")
        .unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO alarms (time, days_of_week, enabled) VALUES ('11:10', '1111111', 1);",
        )
        .unwrap();
    let (status, alarms) = request(&state, "GET", "/api/v1/alarms", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(alarms[0]["timezone"], "Europe/Paris");
    let next = chrono::DateTime::parse_from_rfc3339(
        alarms[0]["next_fired_at"]
            .as_str()
            .expect("prochaine occurrence manquante"),
    )
    .unwrap();
    use chrono::Timelike;
    assert_eq!((next.hour(), next.minute()), (11, 10));
}

#[tokio::test]
async fn arret_8838_api_annonce_le_meme_fuseau_et_la_prochaine_heure() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("alarms_timezone", "Europe/Paris")
        .unwrap();
    let reglage =
        serde_json::from_value(json!({"enabled":true,"time":"23:00","days_of_week":"1111111"}))
            .unwrap();
    tune_core::arret_programme::ecrire(&state.backend, reglage).unwrap();
    let (status, body) = request(
        &state,
        "GET",
        "/api/v1/system/settings/scheduled-stop",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["timezone"], "Europe/Paris");
    let next =
        chrono::DateTime::parse_from_rfc3339(body["next_stop_at"].as_str().unwrap()).unwrap();
    use chrono::Timelike;
    assert_eq!((next.hour(), next.minute()), (23, 0));
}
