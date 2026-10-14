//! #6062 — FabienM, fil 2199 (rc3, Linux) : zone « Enfants » en Chromecast,
//! une piste en pause depuis 17 min, puis sept reprises en une minute et pas
//! un son, sans une ligne au journal.
//!
//! Cause : `ChromecastOutput::resume()` ne relançait le média que si le
//! récepteur montrait encore notre application ET une entrée média ; sinon il
//! rendait `Ok(())` sans rien envoyer. La sortie dit désormais qu'elle a perdu
//! sa session (refus + `device_released_on_pause()`), et `resume` rétablit
//! alors la lecture à la position conservée, comme pour un périphérique rendu
//! (#4177).
//!
//! Le banc : le VRAI `resume`, une zone « chromecast » en pause à 0:40 sur
//! une piste Qobuz dont la session de flux est VIVANTE (rien à rétablir côté
//! Tune), et une sortie factice qui refuse la reprise comme le récepteur
//! refermé.
//!
//! Contre-épreuve : retirer le second passage de `reprendre` (le bras
//! `if !apres_refus_de_la_sortie && out.device_released_on_pause()`) fait
//! tomber `une_sortie_sans_session_a_la_reprise_retablit_la_piste_a_sa_position`.
use super::PlaybackOrchestrator;
use super::reprise_dlna_position_2095::QobuzDeBanc;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "chromecast-2fb80b7c-enfants-6062";
const POSITION_PAUSE_MS: u64 = 40_000;
const DUREE_MS: i64 = 245_000;

/// Zone Chromecast en pause à [`POSITION_PAUSE_MS`], session de flux vivante.
/// `perdue` : la sortie refusera la reprise faute de session.
async fn zone_cast_en_pause(perdue: bool) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzDeBanc));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Enfants", Some("chromecast"), Some(APPAREIL))
        .unwrap();
    let sortie = MockOutput::new(APPAREIL, "Enfants").with_type("chromecast");
    if perdue {
        sortie.perdre_la_session_a_la_reprise();
    }
    orch.outputs.lock().await.register(Box::new(sortie));
    let sid = orch
        .streamer
        .create_proxy_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            "https://127.0.0.1:9/file/6062.flac".into(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Fyrsta".into(),
                source: "qobuz".into(),
                source_id: Some("6062".into()),
                stream_id: Some(sid.clone()),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
    orch.playback
        .update_position(zone_id, POSITION_PAUSE_MS as i64)
        .await;
    orch.playback.pause(zone_id).await;
    assert!(
        orch.streamer.session_alive(&sid).await,
        "prémisse : côté Tune, la session de flux est vivante"
    );
    (orch, zone_id)
}

/// `(resume reçus, play_media reçus)` par la sortie.
async fn appels_de_la_sortie(orch: &PlaybackOrchestrator) -> (u64, usize) {
    let arc = { orch.outputs.lock().await.get(APPAREIL) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.resume_call_count(), mock.play_call_count().await)
}

/// LE défaut : la sortie n'a plus rien à reprendre. La reprise doit
/// rétablir la piste à 0:40 au lieu de rester muette (ou d'échouer).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_sortie_sans_session_a_la_reprise_retablit_la_piste_a_sa_position() {
    let (orch, zone_id) = zone_cast_en_pause(true).await;

    let resultat = orch.resume(zone_id, Some(APPAREIL)).await;

    assert!(
        resultat.is_ok(),
        "la reprise d'une sortie qui a perdu sa session doit rétablir la lecture (fil 2199) : {resultat:?}"
    );
    let (reprises, lectures) = appels_de_la_sortie(&orch).await;
    assert_eq!(reprises, 1, "UN essai sur place, pas de boucle");
    assert_eq!(lectures, 1, "la piste est rechargée sur la sortie");
    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(etat.state, PlayState::Playing, "la zone joue");
    assert_eq!(
        etat.position_ms, POSITION_PAUSE_MS as i64,
        "à la position de la pause, pas à 0:00"
    );
}

/// Témoin : une sortie qui reprend normalement ne recharge rien.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_sortie_qui_reprend_sur_place_ne_recharge_rien() {
    let (orch, zone_id) = zone_cast_en_pause(false).await;

    orch.resume(zone_id, Some(APPAREIL))
        .await
        .expect("reprise ordinaire");

    let (reprises, lectures) = appels_de_la_sortie(&orch).await;
    assert_eq!((reprises, lectures), (1, 0));
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
}
