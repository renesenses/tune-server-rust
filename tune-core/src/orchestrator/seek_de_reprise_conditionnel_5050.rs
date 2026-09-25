//! #5050 — le `Seek` qui suit une reprise DLNA/OpenHome n'est plus envoyé
//! qu'à un renderer qui n'est pas à la position de la pause.
//!
//! Pourquoi ce Seek existe (d01986a8, JP, 26/06/2026) : des renderers
//! anciens — le Cyrus Stream X — repartent du DÉBUT au Play d'après Pause.
//! Pourquoi il devient conditionnel : sur le Beosound Stage de FabienM (fil
//! 1943), ce Seek superflu fait passer l'appareil en `TRANSITIONING`, et la
//! Pause suivante est refusée en 701 (voir PR #5063 pour la tolérance côté
//! Pause).
//!
//! Le renderer est le factice UPnP de `dlna_pause_701_tests_5050.rs`, servi
//! par un vrai `DlnaOutput` enregistré dans l'orchestrateur : c'est le chemin
//! de production de `resume`, tâche détachée comprise, qui décide.
//!
//! Contre-épreuve : rendre à `detacher_le_seek_apres_reprise` son Seek
//! inconditionnel fait tomber
//! `un_renderer_qui_reprend_en_place_ne_recoit_aucun_seek`.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::session::{ECART_TOLERE_APRES_REPRISE_MS, RESUME_OUTPUT_SEEK_SETTLE_MS};
use super::{PlaybackOrchestrator, seek_de_reprise_necessaire};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputTarget;
use crate::outputs::dlna::pause_701_tests_5050::{Renderer, Scenario, compte_dans, renderer};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

/// La position de la pause : 1:20,741.
const POSITION_PAUSE_MS: u64 = 80_741;

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

/// Met une zone DLNA en pause à `POSITION_PAUSE_MS` sur le renderer factice
/// qui déclarera `rel_time`, la reprend, laisse passer la pose de la tâche
/// détachée, et rend les actions SOAP reçues.
async fn reprendre(rel_time: Option<&'static str>) -> Arc<std::sync::Mutex<Vec<String>>> {
    let Renderer {
        output,
        recues,
        etat,
        task: _serveur,
    } = renderer(Scenario::Fige("PLAYING")).await;
    etat.lock().unwrap().rel_time = rel_time;
    let did = output.device_id().to_string();

    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Parents", Some("dlna"), Some(&did))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(output));
    // Une session de flux vivante : la reprise se fait « sur place ».
    let (sid, _tx, _ready) = orch
        .streamer
        .create_session(
            crate::http::streamer::StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            false,
            4,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(5050),
                title: "Parents".into(),
                source: "qobuz".into(),
                stream_id: Some(sid),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback
        .update_position(zone_id, POSITION_PAUSE_MS as i64)
        .await;
    orch.playback.pause(zone_id).await;

    orch.resume(zone_id, Some(&did))
        .await
        .expect("la reprise doit aboutir");
    assert_eq!(compte_dans(&recues, "Play"), 1, "un Play de reprise");
    // Le seek de reprise est DÉTACHÉ (LAT-P2) : sa pose, puis la lecture de
    // la position et l'éventuel Seek, sur un serveur local.
    tokio::time::sleep(std::time::Duration::from_millis(
        RESUME_OUTPUT_SEEK_SETTLE_MS + 1_500,
    ))
    .await;
    // Le serveur factice vit jusqu'ici (`_serveur`) : toutes les actions de
    // la tâche détachée ont été reçues.
    drop(orch);
    recues
}

/// Le cas du Beosound Stage : le renderer a repris là où il était.
#[tokio::test]
async fn un_renderer_qui_reprend_en_place_ne_recoit_aucun_seek() {
    let recues = reprendre(Some("0:01:21")).await;
    assert!(
        compte_dans(&recues, "GetPositionInfo") >= 1,
        "la tâche doit avoir lu la position : {:?}",
        recues.lock().unwrap()
    );
    assert_eq!(
        compte_dans(&recues, "Seek"),
        0,
        "déjà en place : aucun Seek — {:?}",
        recues.lock().unwrap()
    );
}

/// Le cas d'origine du Seek (Cyrus Stream X) : le renderer repart de zéro.
#[tokio::test]
async fn un_renderer_qui_repart_de_zero_recoit_le_seek() {
    let recues = reprendre(Some("0:00:00")).await;
    assert_eq!(
        compte_dans(&recues, "Seek"),
        1,
        "reparti du début : le Seek doit partir — {:?}",
        recues.lock().unwrap()
    );
}

/// Position illisible : la conduite d'avant, le Seek part.
#[tokio::test]
async fn une_position_illisible_garde_le_seek() {
    let recues = reprendre(None).await;
    assert!(compte_dans(&recues, "GetPositionInfo") >= 1);
    assert_eq!(
        compte_dans(&recues, "Seek"),
        1,
        "sans mesure, on seeke comme avant — {:?}",
        recues.lock().unwrap()
    );
}

/// La règle seule, bornes comprises.
#[test]
fn la_tolerance_encadre_la_position_de_la_pause() {
    let p = POSITION_PAUSE_MS;
    let t = ECART_TOLERE_APRES_REPRISE_MS;
    assert!(seek_de_reprise_necessaire(p, None), "illisible : Seek");
    assert!(!seek_de_reprise_necessaire(p, Some(p)));
    assert!(!seek_de_reprise_necessaire(p, Some(p + t)), "borne haute");
    assert!(!seek_de_reprise_necessaire(p, Some(p - t)), "borne basse");
    assert!(seek_de_reprise_necessaire(p, Some(p + t + 1)));
    assert!(seek_de_reprise_necessaire(p, Some(p - t - 1)));
    assert!(seek_de_reprise_necessaire(p, Some(0)), "reparti de zéro");
    // Le plus petit Seek de reprise tenté (au-delà de 3 s) contre un
    // renderer reparti de zéro et déjà à 1 s : l'écart dépasse la tolérance.
    assert!(seek_de_reprise_necessaire(3_001, Some(1_000)));
}
