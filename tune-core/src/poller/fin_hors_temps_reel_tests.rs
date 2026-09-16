//! #4213: exercise the real poller with an output whose work outlasts the media.
use super::*;
use crate::db::zone_repo::ZoneRepo;
use crate::playback::{NowPlaying, PlayState};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Sortie {
    status: Arc<std::sync::Mutex<OutputStatus>>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl OutputTarget for Sortie {
    fn name(&self) -> &str {
        "sortie de test"
    }
    fn device_id(&self) -> &str {
        "test:completion"
    }
    fn output_type(&self) -> &str {
        "test"
    }
    fn supports_internal_gapless(&self) -> bool {
        false
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn seek(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(self.status.lock().unwrap().clone())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    poller: PositionPoller,
    status: Arc<std::sync::Mutex<OutputStatus>>,
    stops: Arc<AtomicUsize>,
    zone: i64,
    polls: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn new(realtime: bool, position_ms: u64) -> Self {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Test", Some("test"), Some("test:completion"))
            .unwrap();
        let status = Arc::new(std::sync::Mutex::new(OutputStatus {
            state: TransportState::Playing,
            position_ms,
            duration_ms: 240_000,
            realtime,
            track_title: Some("Piste en traitement".into()),
            ..Default::default()
        }));
        let stops = Arc::new(AtomicUsize::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(Sortie {
            status: status.clone(),
            stops: stops.clone(),
        }));
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(crate::http::streamer::AudioStreamer::new(0)),
            Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db,
            Arc::new(Mutex::new(HashMap::new())),
        );
        playback
            .play(
                zone,
                NowPlaying {
                    title: "Piste en traitement".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone, 0, 1).await;
        let mut ps = ZonePollState::new(playback.get_state(zone).await.track_generation);
        ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(600));
        ps.track_loaded_at = Instant::now() - Duration::from_secs(600);
        ps.peak_position_ms = position_ms;
        ps.last_position_ms = position_ms;
        Self {
            poller,
            status,
            stops,
            zone,
            polls: HashMap::from([(zone, ps)]),
            idle: HashMap::new(),
        }
    }
    async fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.poller
                .tick(&mut self.polls, &mut self.idle, &Instant::now())
                .await;
        }
    }
}

#[tokio::test]
async fn une_sortie_hors_temps_reel_attend_sa_fin_effective() {
    // Both the exclusive-output tolerance and the ordinary +3s margin.
    for position in [239_999, 250_000] {
        let mut b = Banc::new(false, position).await;
        b.ticks(8).await;
        assert_eq!(
            b.poller.playback.get_state(b.zone).await.state,
            PlayState::Playing
        );
        assert_eq!(
            b.stops.load(Ordering::SeqCst),
            0,
            "processing must not be cancelled at {position}"
        );
        {
            let mut status = b.status.lock().unwrap();
            status.state = TransportState::Stopped;
            status.ended_naturally = true;
        }
        b.ticks(1).await;
        assert_eq!(
            b.poller.playback.get_state(b.zone).await.state,
            PlayState::Stopped
        );
        let stops = b.stops.load(Ordering::SeqCst);
        b.ticks(5).await;
        assert_eq!(
            b.stops.load(Ordering::SeqCst),
            stops,
            "completion happens only once"
        );
    }
}

#[tokio::test]
async fn une_sortie_temps_reel_conserve_ses_fins_de_secours() {
    for position in [239_999, 250_000] {
        let mut b = Banc::new(true, position).await;
        b.ticks(8).await;
        assert_eq!(
            b.poller.playback.get_state(b.zone).await.state,
            PlayState::Stopped,
            "the realtime fallback must still end the track at {position}"
        );
    }
}

#[test]
fn le_modele_ne_confond_pas_duree_et_fin_hors_temps_reel() {
    let i = fsm::PlayingInput {
        realtime: false,
        gapless_advance_pending: false,
        has_next: false,
        gapless_sent: false,
        track_duration_ms: 240_000,
        reported_duration_ms: 240_000,
        played_enough: true,
        position_ms: 250_000,
        past_end_ticks: 8,
        gapless_enabled: false,
        is_dlna: false,
        wall_elapsed_secs: 600,
    };
    assert!(!fsm::classify_playing(&i).past_end_track_ended);
    assert!(
        fsm::classify_playing(&fsm::PlayingInput {
            realtime: true,
            ..i
        })
        .past_end_track_ended
    );
}
