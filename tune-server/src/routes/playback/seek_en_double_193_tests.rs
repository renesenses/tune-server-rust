//! Ticket 193 — le même `Seek` reçu deux fois coup sur coup ne part qu'une
//! fois vers la sortie.
//!
//! Contre-épreuve : sans la garde de la route, le second `POST /seek` repasse
//! par l'orchestrateur et repose la position de lecture ;
//! `le_second_seek_identique_ne_repasse_pas_par_l_orchestrateur` rougit sur
//! la position.

use super::*;
use axum::body::Body;
use axum::http::Request;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

#[test]
fn la_regle_ne_retient_que_la_meme_position_dans_la_fenetre() {
    let mut registre = HashMap::new();
    let t0 = Instant::now();
    assert!(
        !seek_en_double(&mut registre, 11, 48_976, t0),
        "le premier passe"
    );
    assert!(
        seek_en_double(&mut registre, 11, 48_976, t0 + Duration::from_millis(149)),
        "le même à 149 ms est un doublon (ticket 193)"
    );
    assert!(
        !seek_en_double(&mut registre, 12, 48_976, t0 + Duration::from_millis(150)),
        "une autre zone n'est pas concernée"
    );
    assert!(
        !seek_en_double(&mut registre, 11, 50_000, t0 + Duration::from_millis(200)),
        "une autre position passe"
    );
    assert!(
        !seek_en_double(&mut registre, 11, 48_976, t0 + Duration::from_millis(300)),
        "revenir à la première position après en avoir visé une autre passe"
    );
    assert!(
        !seek_en_double(
            &mut registre,
            11,
            48_976,
            t0 + Duration::from_millis(300) + FENETRE_SEEK_EN_DOUBLE
        ),
        "la même position au-delà de la fenêtre passe"
    );
}

async fn poster_seek(app: &axum::Router, zone_id: i64, position_ms: i64) -> StatusCode {
    let requete = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/zones/{zone_id}/seek"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({ "position_ms": position_ms }).to_string(),
        ))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let _ = axum::body::to_bytes(reponse.into_body(), 1 << 20).await;
    statut
}

#[tokio::test]
async fn le_second_seek_identique_ne_repasse_pas_par_l_orchestrateur() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    state
        .playback
        .restore_position(
            zone_id,
            0,
            tune_core::playback::NowPlaying {
                track_id: Some(1),
                title: "Piste".into(),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;

    assert_eq!(poster_seek(&app, zone_id, 48_976).await, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone_id).await.position_ms, 48_976);

    // La lecture avance entre les deux requêtes : si le doublon repassait par
    // l'orchestrateur, il reposerait la position à 48 976.
    state.playback.seek(zone_id, 49_100).await;
    assert_eq!(
        poster_seek(&app, zone_id, 48_976).await,
        StatusCode::OK,
        "le doublon répond comme le premier"
    );
    assert_eq!(
        state.playback.get_state(zone_id).await.position_ms,
        49_100,
        "le doublon n'a pas été transmis"
    );

    // Une autre position passe, elle.
    assert_eq!(poster_seek(&app, zone_id, 120_000).await, StatusCode::OK);
    assert_eq!(state.playback.get_state(zone_id).await.position_ms, 120_000);
}
