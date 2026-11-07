//! #7894 (point 9 de #7778) — une Pause sur un renderer DLNA qui ne joue rien
//! (`NO_MEDIA_PRESENT`) ne remonte plus en échec : la zone est ARRÊTÉE, sa
//! piste et sa position gardées pour la reprise, et l'appelant apprend que
//! rien ne jouait sur l'appareil.
//!
//! Le renderer est le factice UPnP de `dlna_pause_701_tests_5050.rs`, figé en
//! `NO_MEDIA_PRESENT` : il refuse toute Pause en 701, comme l'appareil du
//! testeur. Un vrai `DlnaOutput` est enregistré dans l'orchestrateur : c'est
//! le chemin de production de `pause` qui décide.
//!
//! Contre-épreuve : ranger de nouveau `NO_MEDIA_PRESENT` dans `Abandonner`
//! (`conduite_apres_refus_pause`) fait tomber
//! `une_pause_sur_un_renderer_sans_media_arrete_la_zone_sans_echec` sur
//! « la pause sur un appareil sans média ne doit pas échouer ».

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{IssueDePause, PlaybackOrchestrator};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputTarget;
use crate::outputs::dlna::pause_701_tests_5050::{Renderer, Scenario, compte_dans, renderer};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

const POSITION_MS: u64 = 42_000;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// La sortie seule : la Pause refusée par un renderer sans média rend `Ok`,
/// le signale une fois, et n'envoie aucun `Stop` de repli.
#[tokio::test]
async fn la_sortie_signale_qu_aucun_media_ne_jouait() {
    let r = renderer(Scenario::Fige("NO_MEDIA_PRESENT")).await;
    r.output
        .pause()
        .await
        .expect("sans média : rien à mettre en pause, ce n'est pas un échec");
    assert!(
        r.output.take_nothing_playing_on_pause(),
        "la sortie doit dire que rien ne jouait"
    );
    assert!(
        !r.output.take_nothing_playing_on_pause(),
        "le signal se lit une seule fois"
    );
    assert_eq!(compte_dans(&r.recues, "Stop"), 0);
}

/// Un renderer arrêté (`STOPPED`, #5050) garde son comportement : refus
/// remonté, aucun signal « rien ne jouait ».
#[tokio::test]
async fn un_renderer_arrete_ne_devient_pas_un_renderer_sans_media() {
    let r = renderer(Scenario::Fige("STOPPED")).await;
    let _ = r.output.pause().await.expect_err("arrêté : refus inchangé");
    assert!(!r.output.take_nothing_playing_on_pause());
}

/// Le cas de l'issue : zone en lecture, appareil sans média, Pause.
#[tokio::test]
async fn une_pause_sur_un_renderer_sans_media_arrete_la_zone_sans_echec() {
    let Renderer {
        output,
        recues,
        etat: _,
        task: _serveur,
    } = renderer(Scenario::Fige("NO_MEDIA_PRESENT")).await;
    let did = output.device_id().to_string();

    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(&did))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(output));
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Épisode".into(),
                source: "podcast".into(),
                source_id: Some("episode-7894".into()),
                duration_ms: 1_800_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback
        .update_position(zone_id, POSITION_MS as i64)
        .await;

    let issue = orch
        .pause_avec_issue(zone_id, Some(&did))
        .await
        .expect("la pause sur un appareil sans média ne doit pas échouer");
    assert_eq!(issue, IssueDePause::RienNeJouait);

    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Stopped,
        "rien ne jouait : la zone est arrêtée, pas « en lecture » ni « en pause »"
    );
    let np = etat
        .now_playing
        .expect("la piste reste en mémoire pour la reprise");
    assert_eq!(np.source_id.as_deref(), Some("episode-7894"));
    assert_eq!(
        ZoneRepo::with_backend(orch.db.clone())
            .get_last_play_state(zone_id)
            .as_deref(),
        Some("stopped")
    );
    assert_eq!(compte_dans(&recues, "Pause"), 1, "une seule Pause envoyée");
    assert_eq!(
        compte_dans(&recues, "Stop"),
        0,
        "aucun Stop à un appareil sans média"
    );
}

/// Témoin de non-régression : un renderer qui accepte la Pause donne une zone
/// en pause, comme avant.
#[tokio::test]
async fn une_pause_acceptee_met_la_zone_en_pause() {
    let Renderer {
        output,
        recues: _,
        etat: _,
        task: _serveur,
    } = renderer(Scenario::Fige("PLAYING")).await;
    let did = output.device_id().to_string();
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(&did))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(output));
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Épisode".into(),
                source: "podcast".into(),
                duration_ms: 1_800_000,
                ..Default::default()
            },
        )
        .await;

    let issue = orch.pause_avec_issue(zone_id, Some(&did)).await.unwrap();
    assert_eq!(issue, IssueDePause::EnPause);
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Paused
    );
}
