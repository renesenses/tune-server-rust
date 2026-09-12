use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

const DEVICE_ID: &str = "local:dac-usb";

/// La sortie du constat : elle dit « en lecture », sa position ne bouge
/// plus, et elle porte — ou non — un échec sur le canal du poller.
struct SortieLocale {
    echec: std::sync::Mutex<Option<String>>,
    position_ms: u64,
}

#[async_trait::async_trait]
impl OutputTarget for SortieLocale {
    fn name(&self) -> &str {
        "DAC USB"
    }
    fn device_id(&self) -> &str {
        DEVICE_ID
    }
    fn output_type(&self) -> &str {
        "local"
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
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
            state: TransportState::Playing,
            position_ms: self.position_ms,
            duration_ms: 240_000,
            ..Default::default()
        })
    }
    fn take_output_failure(&self) -> Option<String> {
        self.echec.lock().unwrap().take()
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

impl Banc {
    /// Une zone locale en lecture sur `DEVICE_ID`, dont la sortie porte
    /// `echec` (ou rien du tout pour le témoin vert).
    async fn monter(echec: Option<&str>) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("local"), Some(DEVICE_ID))
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(SortieLocale {
            echec: std::sync::Mutex::new(echec.map(|e| e.to_string())),
            // La position du constat : deux secondes, et plus rien.
            position_ms: 2_000,
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
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);

        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "Never Make It on Time".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;

        Self {
            poller,
            playback,
            zone_id,
            recu,
        }
    }

    async fn un_tick(&self) {
        let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();
        let mut idle_backoff: HashMap<i64, IdlePollBackoff> = HashMap::new();
        self.poller
            .tick(&mut poll_states, &mut idle_backoff, &Instant::now())
            .await;
    }

    /// Ce que l'écran reçoit pour cette zone : le message, et son `fatal`.
    fn erreur(&mut self) -> Option<(String, bool)> {
        let mut trouvee = None;
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.playback_error"
                && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(self.zone_id)
            {
                trouvee = Some((
                    ev.data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ev.data.get("fatal").and_then(|v| v.as_bool()) == Some(true),
                ));
            }
        }
        trouvee
    }
}

/// LE FAIT DE BASE : une sortie qui a posé un échec sur son canal fait
/// parler l'écran, et la zone s'arrête. Sans le `bus.emit` de `tick()`,
/// ce test tombe.
#[tokio::test]
async fn un_echec_de_sortie_atteint_l_ecran_et_arrete_la_zone() {
    let mut banc = Banc::monter(Some(
        "Sortie « DAC USB » : le périphérique a accepté l'ouverture CoreAudio \
         puis a cessé de recevoir l'audio ; la lecture est restée figée à 2000 ms.",
    ))
    .await;

    banc.un_tick().await;

    let (message, fatal) = banc
        .erreur()
        .expect("un échec de sortie doit atteindre l'écran");
    assert!(
        message.contains("DAC USB") && message.contains("2000 ms"),
        "le message doit dire la cause, pas « une erreur est survenue » : {message}"
    );
    assert!(
        fatal,
        "sans `fatal`, la fenêtre de grâce du client avale le message et \
         l'utilisateur n'a, une fois de plus, que le silence"
    );
    assert_ne!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "une zone dont la sortie a lâché ne doit pas rester « en lecture »"
    );
}

/// TÉMOIN VERT : la même zone, la même sortie, la même position figée — mais
/// aucun échec posé. Rien ne doit être dit, et la lecture continue.
/// C'est la garde contre un correctif qui crierait au loup.
#[tokio::test]
async fn une_sortie_sans_echec_ne_dit_rien_et_continue_de_jouer() {
    let mut banc = Banc::monter(None).await;

    banc.un_tick().await;

    assert!(
        banc.erreur().is_none(),
        "aucun échec posé : rien ne doit remonter à l'écran"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "une lecture saine ne doit pas être coupée"
    );
}

/// Le canal est à usage unique : le tick suivant ne re-coupe pas une zone
/// que l'utilisateur vient de relancer.
#[tokio::test]
async fn un_echec_deja_remonte_ne_recoupe_pas_la_lecture_suivante() {
    let mut banc = Banc::monter(Some("Sortie « DAC USB » : figée à 2000 ms.")).await;
    banc.un_tick().await;
    assert!(banc.erreur().is_some(), "premier tick : le message part");

    banc.playback
        .play(
            banc.zone_id,
            NowPlaying {
                title: "La piste d'après".into(),
                source: "local".into(),
                duration_ms: 240_000,
                ..Default::default()
            },
        )
        .await;
    banc.un_tick().await;

    assert!(
        banc.erreur().is_none(),
        "un échec ne doit jamais être remonté deux fois"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "la piste relancée ne doit pas mourir de l'erreur de la précédente"
    );
}
