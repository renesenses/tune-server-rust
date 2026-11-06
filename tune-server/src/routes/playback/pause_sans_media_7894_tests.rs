//! #7894 (point 9 de #7778) — Pause sur un appareil qui ne joue rien : la
//! route ne rend plus 502, elle rend la zone ARRÊTÉE et une phrase traduite
//! qui dit que rien ne jouait sur l'appareil.
//!
//! L'appareil est un `MockOutput` de type `dlna` armé « sans média »
//! (`with_sans_media`) : sa pause n'a rien à suspendre et il le signale par
//! `take_nothing_playing_on_pause()`, comme le `DlnaOutput` devant un
//! renderer en `NO_MEDIA_PRESENT` (témoins de tune-core,
//! `orchestrator/pause_sans_media_7894_tests.rs`). Aucun son.
//!
//! Contre-épreuve : dans `PlaybackOrchestrator::pause_avec_issue`, ne plus
//! lire `take_nothing_playing_on_pause()` (forcer `false`) fait tomber
//! `une_pause_sans_media_rend_une_zone_arretee_et_le_dit` : la zone est
//! annoncée « paused ».

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::{NowPlaying, PlayState};

use crate::state::AppState;

const APPAREIL: &str = "uuid:salon-sans-media-7894";

async fn zone_sur_appareil_sans_media() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon DLNA", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon")
            .with_type("dlna")
            .with_sans_media(),
    ));
    state
        .playback
        .play(
            zid,
            NowPlaying {
                title: "Épisode".into(),
                source: "podcast".into(),
                source_id: Some("https://h.example/media/42".into()),
                duration_ms: 1_800_000,
                ..Default::default()
            },
        )
        .await;
    state.playback.update_position(zid, 42_000).await;
    (state, zid)
}

async fn pause(state: &AppState, zid: i64, langue: &str) -> (StatusCode, Value) {
    let reponse = crate::routes::router(state.clone())
        .oneshot(
            Request::post(format!("/api/v1/zones/{zid}/pause"))
                .header("accept-language", langue)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), 1024 * 1024).await.unwrap();
    (statut, serde_json::from_slice(&octets).unwrap())
}

#[tokio::test]
async fn une_pause_sans_media_rend_une_zone_arretee_et_le_dit() {
    let (state, zid) = zone_sur_appareil_sans_media().await;
    let (statut, corps) = pause(&state, zid, "fr").await;

    assert_eq!(statut, StatusCode::OK, "plus de 502 : {corps}");
    assert_eq!(
        corps["state"], "stopped",
        "rien ne jouait : la zone est arrêtée, ni « playing » ni « paused » : {corps}"
    );
    assert_eq!(corps["code"], "nothing_playing_on_device", "{corps}");
    assert_eq!(
        corps["message_key"], "api.playback.nothingPlayingOnDevice",
        "{corps}"
    );
    let message = corps["message"].as_str().unwrap();
    assert!(message.contains("Salon DLNA"), "{message}");
    assert!(message.contains("Rien ne joue"), "{message}");

    // Reprise possible : la piste reste en mémoire.
    let etat = state.playback.get_state(zid).await;
    assert_eq!(etat.state, PlayState::Stopped);
    assert!(
        etat.now_playing.is_some(),
        "la piste est gardée pour la reprise"
    );
}

#[tokio::test]
async fn la_phrase_suit_la_langue_de_l_interface() {
    let (state, zid) = zone_sur_appareil_sans_media().await;
    let (_, corps) = pause(&state, zid, "en").await;
    let message = corps["message"].as_str().unwrap();
    assert!(
        message.starts_with("Nothing is playing on Salon DLNA"),
        "{message}"
    );
}
