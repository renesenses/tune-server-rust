//! #6380 — la « Lecture » qui suit un redémarrage, à la position restaurée,
//! au niveau de l'orchestrateur : [`PlaybackOrchestrator::play_a_la_position_restauree`].
//!
//! La piste de la bibliothèque (session fichier) est éprouvée sur le vrai
//! routeur, dans `tune-server/src/routes/playback/reprise_apres_redemarrage_dlna_6380_tests.rs`.
//! Ici : la piste QOBUZ restaurée, qui part dans une session MANDATAIRE servie
//! depuis l'octet 0 (même règle #2893), et la sortie LOCALE, dont le flux
//! décodé part déjà de l'offset et qui ne doit recevoir aucun `Seek`.
//!
//! Contre-épreuve : retirer l'appel à `seek_output_after_replay` de
//! `play_a_la_position_restauree` fait tomber
//! `une_piste_qobuz_restauree_ramene_le_renderer_a_sa_position`.
use super::reprise_dlna_position_2095::QobuzDeBanc;
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const SOURCE_ID: &str = "24293033";
const POSITION_RESTAUREE_MS: u64 = 7_000;
const DUREE_MS: i64 = 302_973;

/// Une zone arrêtée par le redémarrage, sa piste Qobuz restaurée à
/// [`POSITION_RESTAUREE_MS`] (`restore_service_track_positions`).
async fn zone_redemarree(appareil: &str, type_de_sortie: &str) -> (PlaybackOrchestrator, i64) {
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
        .create("Salon 6380", Some(type_de_sortie), Some(appareil))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "Salon 6380").with_type(type_de_sortie),
    ));
    orch.playback
        .restore_position(
            zone_id,
            POSITION_RESTAUREE_MS as i64,
            NowPlaying {
                title: "Tell Me a Bedtime Story".into(),
                source: "qobuz".into(),
                source_id: Some(SOURCE_ID.into()),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id)
}

/// La demande que bâtit `POST /play` sans corps pour la piste restaurée.
fn demande_de_reprise(zone_id: i64, appareil: &str, seek_ms: Option<u64>) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(appareil.into()),
        track_id: None,
        source: Some("qobuz".into()),
        source_id: Some(SOURCE_ID.into()),
        title: Some("Tell Me a Bedtime Story".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(DUREE_MS),
        seek_ms,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

/// `(play_media reçus, cibles des seek reçus)`, après la pose du `Seek` détaché.
async fn recu_par_la_sortie(orch: &PlaybackOrchestrator, appareil: &str) -> (usize, Vec<u64>) {
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
    let arc = { orch.outputs.lock().await.get(appareil) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.seek_calls())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_piste_qobuz_restauree_ramene_le_renderer_a_sa_position() {
    const APPAREIL: &str = "dlna:uuid-diretta-6380-qobuz";
    let (orch, zone_id) = zone_redemarree(APPAREIL, "dlna").await;
    orch.play_a_la_position_restauree(demande_de_reprise(
        zone_id,
        APPAREIL,
        Some(POSITION_RESTAUREE_MS),
    ))
    .await
    .expect("la reprise doit aboutir");

    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    let sid = np.stream_id.expect("un flux porte la piste");
    assert!(
        orch.streamer.is_seekable_session(&sid).await,
        "prémisse : la piste Qobuz part dans une session mandataire (servie depuis 0)"
    );
    let (lectures, seeks) = recu_par_la_sortie(&orch, APPAREIL).await;
    assert_eq!(lectures, 1, "prémisse : la piste a été jouée");
    assert_eq!(
        seeks,
        vec![POSITION_RESTAUREE_MS],
        "le renderer doit recevoir UN Seek à la position restaurée (#6380)"
    );
}

/// Sans position, c'est `play` : aucun `Seek`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_position_aucun_seek() {
    const APPAREIL: &str = "dlna:uuid-diretta-6380-debut";
    let (orch, zone_id) = zone_redemarree(APPAREIL, "dlna").await;
    orch.play_a_la_position_restauree(demande_de_reprise(zone_id, APPAREIL, None))
        .await
        .expect("la lecture doit aboutir");
    let (lectures, seeks) = recu_par_la_sortie(&orch, APPAREIL).await;
    assert_eq!(lectures, 1, "prémisse : la piste a été jouée");
    assert!(seeks.is_empty(), "aucun Seek sans position : {seeks:?}");
}

/// Une sortie LOCALE garde son chemin : la position passe par
/// `set_pending_start_position_ms` / le flux décodé, jamais par un `Seek`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_sortie_locale_ne_recoit_aucun_seek() {
    const APPAREIL: &str = "local:banc-6380";
    let (orch, zone_id) = zone_redemarree(APPAREIL, "local").await;
    let _ = orch
        .play_a_la_position_restauree(demande_de_reprise(
            zone_id,
            APPAREIL,
            Some(POSITION_RESTAUREE_MS),
        ))
        .await;
    let (_, seeks) = recu_par_la_sortie(&orch, APPAREIL).await;
    assert!(
        seeks.is_empty(),
        "une sortie locale ne reçoit pas de Seek de rattrapage : {seeks:?}"
    );
}
