//! #6264 — Eversolo DMP-A8 : bruit blanc à la reprise après une longue pause.
//! Sur une zone qui arme la reprise par nouveau flux, une pause plus longue
//! que le seuil reprend par une NOUVELLE URI à la position, pas par un `Play`
//! sur place. Les commandes sont comptées SUR LA SORTIE (factice).
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "uuid:eversolo-dmp-a8";

fn ecrire_wav(chemin: &std::path::Path) {
    let octets_data: u32 = 44_100 * 4;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    std::fs::write(chemin, v).unwrap();
}

/// (play_media reçus, resume reçus) après une pause de `pause`.
async fn reprendre_apres(arme: bool, pause: Duration) -> (usize, u64) {
    let dossier = crate::test_scratch::scratch_dir("tune-6264-reprise");
    let fichier = dossier.join("el-nino.wav");
    ecrire_wav(&fichier);
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon DMP-A8", Some("dlna"), Some(APPAREIL))
        .unwrap();
    if arme {
        SettingsRepo::with_backend(db.clone())
            .set(&crate::audio::reprise_nouveau_flux::cle(zone_id), "true")
            .unwrap();
    }
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "DMP-A8").with_type("dlna"),
    ));
    let mut piste = crate::db::models::Track::new("El niño".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("wav".into());
    piste.duration_ms = 1_000;
    let track_id = crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .create(&piste)
        .unwrap();
    let (sid, _tx, _ready) = orch
        .streamer
        .create_session(
            crate::http::streamer::StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
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
                track_id: Some(track_id),
                title: "El niño".into(),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: 1_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_position(zone_id, 500).await;
    orch.playback.pause(zone_id).await;
    orch.playback
        .vieillir_la_pause_pour_test(zone_id, pause)
        .await;

    let _ = orch.resume(zone_id, Some(APPAREIL)).await;

    let outputs = orch.outputs.lock().await;
    let sortie = outputs.get(APPAREIL).unwrap();
    let sortie = sortie.lock().await;
    let mock = sortie
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    (mock.play_call_count().await, mock.resume_call_count())
}

/// 🔴 Le cas du .18 : 27 min de pause, zone armée — un nouveau flux, pas de
/// `Play` sur place.
#[tokio::test]
async fn apres_une_longue_pause_la_zone_armee_reprend_par_un_nouveau_flux_6264() {
    let (lectures, reprises) = reprendre_apres(true, Duration::from_secs(27 * 60)).await;
    assert_eq!(
        (lectures, reprises),
        (1, 0),
        "#6264 : la reprise après 27 min doit relancer le morceau par une \
         nouvelle URI (play_media), pas par un Play sur place que le DMP-A8 \
         rejoue depuis sa mémoire"
    );
}

/// Une pause courte, ou une zone non armée : la reprise sur place d'avant.
#[tokio::test]
async fn pause_courte_ou_zone_non_armee_reprennent_sur_place() {
    assert_eq!(reprendre_apres(true, Duration::from_secs(60)).await, (0, 1));
    assert_eq!(
        reprendre_apres(false, Duration::from_secs(27 * 60)).await,
        (0, 1)
    );
}
