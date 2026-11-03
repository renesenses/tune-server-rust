//! #6332 — le débit d'une piste LOCALE atteint « Lecture en cours » : par la
//! charge utile écrite à la main des retours de transport (`build_zone_json`)
//! comme par la fiche de zone (`GET /zones/{id}`), qui sérialise `NowPlaying`.
//!
//! Contre-épreuve : sans `bitrate_kbps` dans `NowPlaying::from_track`, les deux
//! charges utiles portent `null` et le test rougit sur la première.

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::zone_repo::ZoneRepo;

#[tokio::test]
async fn le_debit_de_la_piste_locale_sort_dans_l_etat_de_zone_6332() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("browser"), None)
        .unwrap();
    let mut t = Track::new("Why".into());
    t.file_path = Some("/m/diva/02-why.mp3".into());
    t.format = Some("mp3".into());
    t.sample_rate = Some(44_100);
    t.bitrate_kbps = Some(128);
    let id = TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .unwrap();
    let ligne = TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .unwrap();
    state
        .playback
        .play(zid, tune_core::playback::NowPlaying::from_track(&ligne))
        .await;

    let retour = build_zone_json(&state, zid).await;
    assert_eq!(
        retour["current_track"]["bitrate_kbps"],
        serde_json::json!(128),
        "la charge utile des retours de transport tait le débit : {}",
        retour["current_track"]
    );

    let app = crate::routes::router(state.clone());
    let rep = app
        .oneshot(
            Request::get(format!("/api/v1/zones/{zid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let corps: serde_json::Value =
        serde_json::from_slice(&to_bytes(rep.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        corps["current_track"]["bitrate_kbps"],
        serde_json::json!(128),
        "la fiche de zone tait le débit : {}",
        corps["current_track"]
    );
}
