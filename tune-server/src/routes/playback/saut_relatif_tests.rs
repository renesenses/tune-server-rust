//! rc15 (fil 3009) — `POST /zones/{id}/seek/relative` : reculer et avancer
//! de N secondes dans un épisode de podcast.
//!
//! Témoin rouge : sur la base sans la route, chaque POST rend 404/405 et la
//! position ne bouge pas.

use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

async fn poster(app: &axum::Router, zone_id: i64, corps: Value) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/zones/{zone_id}/seek/relative"))
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Une zone sans appareil (le serveur seul tient la position) qui joue un
/// épisode de podcast d'une heure, arrêté à `position_ms`.
async fn banc(
    type_de_sortie: &str,
    position_ms: i64,
) -> (crate::state::AppState, axum::Router, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Cuisine", Some(type_de_sortie), None)
        .unwrap();
    state
        .playback
        .restore_position(
            zone_id,
            position_ms,
            tune_core::playback::NowPlaying {
                title: "Épisode 42".into(),
                source: "podcast".into(),
                source_id: Some("https://cdn.example/show/ep42.mp3".into()),
                duration_ms: 3_600_000,
                ..Default::default()
            },
        )
        .await;
    (state, app, zone_id)
}

#[tokio::test]
async fn reculer_de_15_s_ramene_la_position_15_s_en_arriere() {
    let (state, app, zone) = banc("local", 600_000).await;
    let (statut, corps) = poster(&app, zone, json!({ "delta_ms": -15_000 })).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["position_ms"], 585_000);
    assert_eq!(state.playback.get_state(zone).await.position_ms, 585_000);
}

#[tokio::test]
async fn avancer_de_30_s_porte_la_position_30_s_plus_loin() {
    let (state, app, zone) = banc("local", 600_000).await;
    let (statut, corps) = poster(&app, zone, json!({ "delta_ms": 30_000 })).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(state.playback.get_state(zone).await.position_ms, 630_000);
}

#[tokio::test]
async fn reculer_au_debut_s_arrete_a_zero_et_avancer_a_la_fin_a_la_derniere_seconde() {
    let (state, app, zone) = banc("local", 5_000).await;
    let (statut, _) = poster(&app, zone, json!({ "delta_ms": -15_000 })).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone).await.position_ms, 0);

    let (state, app, zone) = banc("local", 3_590_000).await;
    let (statut, _) = poster(&app, zone, json!({ "delta_ms": 30_000 })).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        state.playback.get_state(zone).await.position_ms,
        3_599_000,
        "une seconde avant la fin, pas au-delà"
    );
}

#[tokio::test]
async fn un_saut_hors_de_1_a_99_s_est_refuse_sans_rien_bouger() {
    let (state, app, zone) = banc("local", 600_000).await;
    for delta in [0, 999, -999, 99_001, -100_000, i64::MIN] {
        let (statut, corps) = poster(&app, zone, json!({ "delta_ms": delta })).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "delta {delta} : {corps}");
        assert_eq!(corps["error"], "invalid_delta_ms");
    }
    assert_eq!(state.playback.get_state(zone).await.position_ms, 600_000);
    // Les bornes, elles, passent.
    let (statut, _) = poster(&app, zone, json!({ "delta_ms": 99_000 })).await;
    assert_eq!(statut, StatusCode::OK);
    let (statut, _) = poster(&app, zone, json!({ "delta_ms": -1_000 })).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone).await.position_ms, 698_000);
}

#[tokio::test]
async fn une_zone_navigateur_part_de_la_position_que_joue_l_onglet() {
    // Le serveur croit l'onglet à 0 ; l'onglet joue à 120 s.
    let (state, app, zone) = banc("browser", 0).await;
    let (statut, _) = poster(
        &app,
        zone,
        json!({ "delta_ms": 30_000, "position_ms": 120_000 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone).await.position_ms, 150_000);
}

#[tokio::test]
async fn ailleurs_la_position_du_client_est_ignoree() {
    let (state, app, zone) = banc("local", 600_000).await;
    let (statut, _) = poster(
        &app,
        zone,
        json!({ "delta_ms": 30_000, "position_ms": 120_000 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone).await.position_ms, 630_000);
}
