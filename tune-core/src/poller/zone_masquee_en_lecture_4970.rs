//! #4970 — une zone MASQUÉE qui joue doit rester suivie par le sondeur.
//!
//! Terrain (Stéphane Villerio, DMP-A6, 0.9.164, fil 1931) : la zone DLNA 11
//! a été masquée par l'ignorance d'un appareil voisin (#4957), mais reste
//! jouable. Le sondeur lisait ses zones par `ZoneRepo::list()`, qui ne rend
//! que les zones visibles : type de sortie lu `""`, donc `is_dlna = false`,
//! et aucun filet de fin DLNA ne pouvait conclure. Le renderer gèle sa
//! position SUR la durée ; `past_end` n'arrive donc jamais : l'album
//! s'arrête après chaque piste (`gapless_arm_trace output=""`).
//!
//! Le banc : zone DLNA en lecture, file de trois pistes, le renderer gèle sa
//! position à sa durée. On compare la zone visible (témoin de contrôle) et
//! la même zone masquée : dans les deux cas le sondeur doit prononcer la fin
//! et relancer la piste 2.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:uuid:3E151150-D9C0-11F0-A7C6-800A805C2689";
const DUREE_FILE_MS: i64 = 237_651;
const DUREE_RENDERER_MS: u64 = 237_000;
const POSITION_GELEE_MS: u64 = 237_000;
const HORLOGE_A_LA_FIN_SECS: u64 = 243;

const FINIE: &str = "Speak to Me/Breathe";
const SUIVANTE: &str = "On the Run";

fn ecrire_wav(chemin: &std::path::Path) {
    use std::io::Write;
    let octets_data: u32 = 44_100 * 4 / 5;
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
    let mut f = std::fs::File::create(chemin).unwrap();
    f.write_all(&v).unwrap();
    f.flush().unwrap();
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    _fichiers: Vec<tempfile::NamedTempFile>,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    /// Zone DLNA en lecture de `FINIE`, file `[FINIE, SUIVANTE, …]`. Si
    /// `masquee`, la zone est masquée (`is_hidden = 1`) APRÈS le lancement,
    /// comme par l'ignorance d'un appareil en pleine lecture.
    async fn monter(masquee: bool) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);

        let depot_zones = ZoneRepo::with_backend(db.clone());
        let zone_id = depot_zones
            .create("DMP-A6", Some("dlna"), Some(APPAREIL))
            .unwrap();

        let depot = TrackRepo::with_backend(db.clone());
        let mut fichiers = Vec::new();
        let mut pistes = Vec::new();
        for (n, (titre, duree)) in [
            (FINIE, DUREE_FILE_MS),
            (SUIVANTE, 212_000),
            ("Time", 200_000),
        ]
        .iter()
        .enumerate()
        {
            let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
            ecrire_wav(f.path());
            let mut piste = crate::db::models::Track::new((*titre).to_string());
            piste.file_path = Some(f.path().to_str().unwrap().to_string());
            piste.format = Some("wav".into());
            piste.sample_rate = Some(44_100);
            piste.bit_depth = Some(16);
            piste.channels = 2;
            piste.track_number = n as i32 + 1;
            piste.duration_ms = *duree;
            pistes.push(depot.create(&piste).unwrap());
            fichiers.push(f);
        }
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes)
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "DMP-A6").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let streamer = Arc::new(AudioStreamer::new(0));
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            streamer.clone(),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator.clone(),
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        let flux = streamer
            .create_file_session(
                StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    ..Default::default()
                },
                fichiers[0].path().to_string_lossy().into_owned(),
                false,
            )
            .await;
        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(pistes[0]),
                    title: FINIE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_FILE_MS,
                    stream_id: Some(flux),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 3).await;
        playback
            .dater_le_demarrage(zone_id, Duration::from_secs(HORLOGE_A_LA_FIN_SECS))
            .await;

        if masquee {
            depot_zones.delete(zone_id).unwrap();
            assert!(
                !depot_zones
                    .list()
                    .unwrap()
                    .iter()
                    .any(|z| z.id == Some(zone_id)),
                "prémisse : la zone masquée n'est plus dans `list()`"
            );
        }

        let generation = playback.get_state(zone_id).await.track_generation;
        let mut poll_states = HashMap::new();
        poll_states.insert(zone_id, ZonePollState::new(generation));

        Self {
            poller,
            playback,
            outputs,
            zone_id,
            _fichiers: fichiers,
            poll_states,
            idle: HashMap::new(),
        }
    }

    /// Le DMP-A6 gèle sa position SUR sa durée et dit toujours `Playing`.
    async fn renderer_gele_a_la_fin(&mut self) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(APPAREIL).unwrap();
            let sortie = arc.lock().await;
            let m = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            m.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            m.set_duration(DUREE_RENDERER_MS);
            m.set_position(POSITION_GELEE_MS);
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(HORLOGE_A_LA_FIN_SECS));
        ps.peak_position_ms = ps.peak_position_ms.max(POSITION_GELEE_MS);
        ps.last_position_ms = POSITION_GELEE_MS;
    }

    /// Sonde jusqu'à ce que la piste suivante parte, au plus `max` fois.
    async fn sonder_jusqu_a_la_suivante(&mut self, max: usize) -> Vec<String> {
        for _ in 0..max {
            self.renderer_gele_a_la_fin().await;
            self.poller
                .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
                .await;
            let partis = {
                let reg = self.outputs.lock().await;
                let arc = reg.get(APPAREIL).unwrap();
                let sortie = arc.lock().await;
                let m = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
                m.play_titles().await
            };
            if !partis.is_empty() {
                return partis;
            }
        }
        Vec::new()
    }
}

/// Contrôle : la zone VISIBLE, gelée à la fin, passe à la piste 2.
#[tokio::test]
async fn zone_visible_gelee_a_la_fin_passe_a_la_suivante() {
    let mut banc = Banc::monter(false).await;
    let partis = banc.sonder_jusqu_a_la_suivante(10).await;
    assert_eq!(partis, vec![SUIVANTE.to_string()]);
    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(etat.queue_position, 1);
}

/// 🔴 #4970 — la MÊME zone, masquée en pleine lecture : le sondeur doit la
/// suivre comme une zone DLNA et passer à la piste 2. Avant le correctif,
/// il la lisait `output=""` et ne concluait jamais.
#[tokio::test]
async fn zone_masquee_en_lecture_passe_a_la_suivante() {
    let mut banc = Banc::monter(true).await;
    let partis = banc.sonder_jusqu_a_la_suivante(10).await;
    assert_eq!(
        partis,
        vec![SUIVANTE.to_string()],
        "une zone masquée qui joue doit rester suivie : fin prononcée, piste 2 lancée"
    );
    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(etat.queue_position, 1);
}
