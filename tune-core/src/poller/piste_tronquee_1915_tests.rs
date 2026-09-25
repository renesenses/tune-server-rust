//! Fil 1915 (Reivax66) — une piste dont le flux s'est coupé loin de sa fin :
//! le sondeur PASSE À LA SUIVANTE en le disant, il n'arrête plus la zone
//! (décision de Bertrand, 24/09/2026). Dernière piste de la file : fin de
//! file, avec le message.
//!
//! La sortie locale pose le constat préfixé `PREFIXE_PISTE_TRONQUEE` sur le
//! canal `take_output_failure` (voir `outputs/local/piste_tronquee_1915.rs`).
//! Ces témoins partent de ce constat et traversent le VRAI `tick()`.

use super::{IdlePollBackoff, PositionPoller, ZonePollState};
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::{AutoplayMode, ZoneRepo};
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{PlayState, PlaybackManager};
use crate::poller::decisions::PREFIXE_PISTE_TRONQUEE;
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

const DEVICE_ID: &str = "mock-smart-dx1";
const MESSAGE: &str = "Sortie « Smart DX1 » : le flux de la piste s'est interrompu à 7:50 sur \
                       11:51 ; la piste a été abandonnée.";

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    _fichiers: tempfile::TempDir,
}

/// Ce que l'écran a reçu pour la zone.
#[derive(Default)]
struct Recu {
    erreurs: Vec<(String, Option<bool>)>,
    sauts: Vec<serde_json::Value>,
}

impl Banc {
    /// Une file locale de deux pistes WAV, lecture lancée à `position`, et la
    /// sortie qui porte `constat` sur son canal d'échec.
    async fn monter(position: i64, constat: &str) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let zones = ZoneRepo::with_backend(db.clone());
        let zone_id = zones
            .create("Salon", Some("mock"), Some(DEVICE_ID))
            .unwrap();
        // Pas d'autoplay : la fin de file doit être une vraie fin de file.
        zones
            .update_autoplay_mode(zone_id, AutoplayMode::Off)
            .unwrap();

        let fichiers = tempfile::tempdir().unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Mingus');
             INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Mingus Ah Um', 1, 1959);",
        )
        .unwrap();
        for (id, titre) in [(1_i64, "R and R"), (2, "La suivante")] {
            let chemin = fichiers.path().join(format!("{id}.wav"));
            let mut wav =
                crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(1000))
                    .to_vec();
            wav.resize(wav.len() + 44100 * 4, 0);
            std::fs::write(&chemin, wav).unwrap();
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, \
                 sample_rate, bit_depth, duration_ms) \
                 VALUES (?, ?, 1, 1, ?, 'wav', 44100, 16, 711666)",
                &[&id, &titre, &chemin.to_str().unwrap()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO queue_items (zone_id, position, track_id, source, duration_ms) \
                 VALUES (?, ?, ?, 'local', 711666)",
                &[&zone_id, &(id - 1), &id],
            )
            .unwrap();
        }

        let sortie = MockOutput::new(DEVICE_ID, "Smart DX1");
        sortie.poser_echec(constat);
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(sortie));

        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        orchestrator
            .play_from_queue(zone_id, position)
            .await
            .expect("la piste de départ doit jouer");
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db,
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);
        Self {
            poller,
            playback,
            zone_id,
            recu,
            _fichiers: fichiers,
        }
    }

    async fn un_tick(&self) {
        let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();
        let mut idle_backoff: HashMap<i64, IdlePollBackoff> = HashMap::new();
        self.poller
            .tick(&mut poll_states, &mut idle_backoff, &Instant::now())
            .await;
    }

    fn recu(&mut self) -> Recu {
        let mut recu = Recu::default();
        while let Ok(ev) = self.recu.try_recv() {
            if ev.data.get("zone_id").and_then(|v| v.as_i64()) != Some(self.zone_id) {
                continue;
            }
            match ev.event_type.as_str() {
                "zone.playback_error" => recu.erreurs.push((
                    ev.data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ev.data.get("fatal").and_then(|v| v.as_bool()),
                )),
                "playback.track_skipped" => recu.sauts.push(ev.data.clone()),
                _ => {}
            }
        }
        recu
    }
}

fn constat() -> String {
    format!("{PREFIXE_PISTE_TRONQUEE}{MESSAGE}")
}

fn verifier_le_message(recu: &Recu) {
    assert_eq!(
        recu.erreurs.len(),
        1,
        "UN message doit partir à l'écran : {:?}",
        recu.erreurs
    );
    let (texte, fatal) = &recu.erreurs[0];
    assert_eq!(
        texte, MESSAGE,
        "le message lisible, sans le préfixe technique du canal"
    );
    assert_eq!(
        *fatal,
        Some(false),
        "une piste coupée n'est pas une panne de sortie : message NON fatal"
    );
    assert_eq!(recu.sauts.len(), 1, "le saut de piste doit être signalé");
    assert_eq!(
        recu.sauts[0].get("reason").and_then(|v| v.as_str()),
        Some(MESSAGE)
    );
}

#[tokio::test]
async fn f1915_coupure_au_milieu_de_la_file_passe_a_la_piste_suivante() {
    let mut banc = Banc::monter(0, &constat()).await;

    banc.un_tick().await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Playing,
        "la zone ne doit plus s'arrêter sur une piste coupée : {etat:?}"
    );
    assert_eq!(
        etat.queue_position, 1,
        "la piste SUIVANTE doit démarrer : {etat:?}"
    );
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.title.as_str()),
        Some("La suivante")
    );
    verifier_le_message(&banc.recu());
}

#[tokio::test]
async fn f1915_coupure_sur_la_derniere_piste_termine_la_file_avec_le_message() {
    let mut banc = Banc::monter(1, &constat()).await;

    banc.un_tick().await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Stopped,
        "dernière piste : fin de file : {etat:?}"
    );
    verifier_le_message(&banc.recu());
}

/// TÉMOIN : un constat de sortie SANS le préfixe garde son chemin d'avant —
/// message fatal, zone arrêtée, aucun saut.
#[tokio::test]
async fn f1915_une_panne_de_sortie_ordinaire_arrete_toujours_la_zone() {
    let mut banc = Banc::monter(0, "Sortie « Smart DX1 » : figée à 2000 ms.").await;

    banc.un_tick().await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_ne!(etat.state, PlayState::Playing, "{etat:?}");
    let recu = banc.recu();
    assert_eq!(recu.erreurs.len(), 1);
    assert_eq!(recu.erreurs[0].1, Some(true));
    assert!(
        recu.sauts.is_empty(),
        "une panne de sortie n'est pas un saut"
    );
}
