//! #6790 — une sortie qui se met en pause d'elle-même (un endpoint OAAT qui a
//! rendu son DAC à un autre lecteur de l'appareil, `endpoint_release`) doit
//! emmener la ZONE en pause, comme une pause de l'utilisateur.
//!
//! Constat de spockfish sur un CM4 + Chord Hugo2 (rc7) : après la libération,
//! la sortie disait `Paused`, mais `GET /api/v1/zones/5` rendait
//! `state: playing`, position figée, et l'interface montrait le bouton pause.

use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;

const DEVICE_ID: &str = "oaat:verox-cm4";

/// Une sortie OAAT dont l'endpoint a — ou non — rendu sa sortie. Elle compte
/// les commandes reçues : la zone doit suivre sans rien lui renvoyer.
struct SortieLiberee {
    liberee: AtomicBool,
    commandes: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl OutputTarget for SortieLiberee {
    fn name(&self) -> &str {
        "cm4"
    }
    fn device_id(&self) -> &str {
        DEVICE_ID
    }
    fn output_type(&self) -> &str {
        "oaat"
    }
    async fn pause(&self) -> Result<(), String> {
        self.commandes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        self.commandes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        self.commandes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        self.commandes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: if self.liberee.load(Ordering::SeqCst) {
                TransportState::Paused
            } else {
                TransportState::Playing
            },
            position_ms: 27_374,
            duration_ms: 240_000,
            ..Default::default()
        })
    }
    fn take_paused_by_device(&self) -> bool {
        self.liberee.swap(false, Ordering::SeqCst)
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
    commandes: Arc<AtomicU32>,
}

impl Banc {
    async fn monter(liberee: bool) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("oaat"), Some(DEVICE_ID))
            .unwrap();

        let commandes = Arc::new(AtomicU32::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(SortieLiberee {
            liberee: AtomicBool::new(liberee),
            commandes: commandes.clone(),
        }));

        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );

        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "Potion".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;

        Self {
            poller,
            playback,
            db,
            zone_id,
            commandes,
        }
    }

    async fn un_tick(&self) {
        let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();
        let mut idle_backoff: HashMap<i64, IdlePollBackoff> = HashMap::new();
        self.poller
            .tick(&mut poll_states, &mut idle_backoff, &Instant::now())
            .await;
    }
}

/// LE CONSTAT : la sortie a rendu son appareil, la zone passe `paused` —
/// en mémoire et en base, comme une pause de l'utilisateur — sans qu'aucune
/// commande ne reparte vers la sortie (ni pause en double, ni relance).
#[tokio::test]
async fn une_sortie_liberee_met_la_zone_en_pause() {
    let banc = Banc::monter(true).await;

    banc.un_tick().await;

    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Paused,
        "#6790 : après endpoint_release la zone doit passer « paused », pas rester « playing »"
    );
    assert_eq!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("paused"),
        "l'état enregistré doit être celui d'une pause"
    );
    assert_eq!(
        banc.commandes.load(Ordering::SeqCst),
        0,
        "la sortie est déjà en pause : la zone la suit sans rien lui renvoyer"
    );
}

/// TÉMOIN VERT : la même zone, sans libération. Elle reste en lecture.
#[tokio::test]
async fn sans_liberation_la_zone_reste_en_lecture() {
    let banc = Banc::monter(false).await;

    banc.un_tick().await;

    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "une sortie qui n'a rien rendu ne doit pas mettre la zone en pause"
    );
}
