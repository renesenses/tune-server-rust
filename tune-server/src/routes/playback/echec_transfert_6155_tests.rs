//! #6155 — la route ne confirme pas un transfert dont la cible refuse.
//! Vraies base/file/route/orchestrateur, sorties factices, aucun son.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{
    models::Track, play_queue_repo::PlayQueueRepo, track_repo::TrackRepo, zone_repo::ZoneRepo,
};
use tune_core::outputs::mock::MockOutput;
use tune_core::outputs::traits::TransportState;
use tune_core::playback::{NowPlaying, PlayState};

const SOURCE: &str = "uuid:source-transfert-6155";
const CIBLE: &str = "uuid:cible-transfert-6155";

async fn transferer(mode: &str, en_pause: bool) -> (StatusCode, Value, PlayState, PlayState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("piste.wav");
    std::fs::write(
        &chemin,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/wav/ref_16_44100_stereo.wav"
        )),
    )
    .unwrap();
    let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zones = ZoneRepo::with_backend(etat.backend.clone());
    let source = zones.create("Source", Some("dlna"), Some(SOURCE)).unwrap();
    let cible = zones.create("Cible", Some("dlna"), Some(CIBLE)).unwrap();
    let mut piste = Track::new("Témoin transfert 6155".into());
    piste.file_path = Some(chemin.to_string_lossy().into_owned());
    piste.format = Some("wav".into());
    piste.sample_rate = Some(44_100);
    piste.bit_depth = Some(16);
    piste.duration_ms = 30_000;
    let id = TrackRepo::with_backend(etat.backend.clone())
        .create(&piste)
        .unwrap();
    PlayQueueRepo::with_backend(etat.backend.clone())
        .set_queue(source, &[id])
        .unwrap();
    let sortie_source = MockOutput::new(SOURCE, "Source").with_type("dlna");
    sortie_source.set_state(TransportState::Playing).await;
    etat.outputs.lock().await.register(Box::new(sortie_source));
    if mode != "absente" {
        let sortie_cible = MockOutput::new(CIBLE, "Cible").with_type("dlna");
        if mode == "refus" {
            sortie_cible.refuser_la_lecture(Some("refus de lecture injecté 6155"));
        }
        etat.outputs.lock().await.register(Box::new(sortie_cible));
    }
    etat.playback
        .play(
            source,
            NowPlaying {
                track_id: Some(id),
                title: piste.title,
                source: "local".into(),
                duration_ms: 30_000,
                ..Default::default()
            },
        )
        .await;
    if en_pause {
        etat.playback.pause(source).await;
    }
    let app = crate::routes::router(etat.clone());
    let reponse = app
        .oneshot(
            Request::post(format!("/api/v1/zones/{source}/transfer"))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"target_zone_id":cible,"stop_source":true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&octets).unwrap();
    (
        code,
        corps,
        etat.playback.get_state(source).await.state,
        etat.playback.get_state(cible).await.state,
    )
}

#[tokio::test]
async fn une_cible_absente_ne_confirme_pas_le_transfert_et_garde_la_source_6155() {
    let (code, corps, source, cible) = transferer("absente", false).await;
    assert_eq!(
        code,
        StatusCode::BAD_GATEWAY,
        "#6155 : cible indisponible, le transfert ne doit ni rendre un faux succès ni arrêter la source : {corps}"
    );
    assert_eq!(corps["error"], "transfer_target_play_failed");
    assert_eq!(
        source,
        PlayState::Playing,
        "l'échec cible doit conserver la lecture source"
    );
    assert_ne!(cible, PlayState::Playing);
}

#[tokio::test]
async fn un_refus_du_renderer_ne_confirme_pas_le_transfert_6155() {
    let (code, corps, source, cible) = transferer("refus", false).await;
    assert_eq!(
        code,
        StatusCode::BAD_GATEWAY,
        "#6155 : PlayResult portant un refus ne doit pas être réduit à Ok : {corps}"
    );
    assert_eq!(corps["error"], "transfer_target_play_failed");
    assert!(
        corps["message"]
            .as_str()
            .unwrap()
            .contains("refus de lecture injecté 6155")
    );
    assert_eq!(source, PlayState::Playing);
    assert_ne!(cible, PlayState::Playing);
}

#[tokio::test]
async fn un_transfert_reussi_arrete_la_source_et_lance_la_cible_6155() {
    let (code, corps, source, cible) = transferer("accepte", false).await;
    assert_eq!(code, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "transferred");
    assert_eq!(source, PlayState::Stopped);
    assert_eq!(cible, PlayState::Playing);
}

#[tokio::test]
async fn un_transfert_depuis_une_pause_garde_la_cible_en_pause_6155() {
    let (code, corps, source, cible) = transferer("accepte", true).await;
    assert_eq!(code, StatusCode::OK, "{corps}");
    assert_eq!(source, PlayState::Stopped);
    assert_eq!(cible, PlayState::Paused);
}
