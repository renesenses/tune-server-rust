//! #4851 — le sondeur enchaîne une zone HQPlayer native par la playlist de
//! HQPlayer, sans arrêt ni relance.
//!
//! Le journal de Steve Taylor (fil 1773) : `gapless_skipped_exclusive_output`
//! dans la fenêtre d'armement, puis `track_end_gap motif=natural_end_after_stopped
//! plancher_ms=5000` et un `PlaylistAdd clear="1"` + `Play` par piste — 4 à 5 s
//! de blanc à chaque changement de piste de *The Dark Side of the Moon*.
//!
//! Ici, la VRAIE `HqplayerOutput` du registre parle à un faux HQPlayer XML
//! (`outputs/hqplayer_enchainement_tests_4851.rs`) ; le sondeur, l'orchestrateur
//! et la file sont les vrais. On mesure ce qui part sur le fil et ce que
//! l'écran affiche. Aucun `sleep` : la fenêtre des 30 s est atteinte en
//! écrivant la position du faux et l'horloge de piste du sondeur.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::hqplayer::enchainement_tests_4851::FauxHqplayer;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DUREE_COURANTE_MS: i64 = 300_000;
const DUREE_SUIVANTE_MS: i64 = 200_000;
const COURANTE: &str = "Speak To Me";
const ARMEE: &str = "Breathe (In The Air)";
const SUITE: &str = "On The Run";
const INSEREE: &str = "Time";
/// Ce que le faux HQPlayer joue au départ — posé par un `play_media`
/// antérieur au banc.
const URI_COURANTE: &str = "http://127.0.0.1:8888/stream/courante.flac";

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
    db: Arc<dyn crate::db::backend::DbBackend>,
    faux: FauxHqplayer,
    zone_id: i64,
    pistes: Vec<i64>,
    _fichiers: Vec<tempfile::NamedTempFile>,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter() -> Self {
        let faux =
            FauxHqplayer::en_lecture(URI_COURANTE, 0.0, DUREE_COURANTE_MS as f64 / 1000.0).await;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);

        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("hqplayer"), Some(&faux.device_id()))
            .unwrap();

        let depot = TrackRepo::with_backend(db.clone());
        let mut fichiers = Vec::new();
        let mut pistes = Vec::new();
        for (n, titre) in [COURANTE, ARMEE, SUITE, INSEREE].iter().enumerate() {
            let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
            ecrire_wav(f.path());
            let mut piste = crate::db::models::Track::new((*titre).to_string());
            piste.file_path = Some(f.path().to_str().unwrap().to_string());
            piste.format = Some("wav".into());
            piste.sample_rate = Some(44_100);
            piste.bit_depth = Some(16);
            piste.channels = 2;
            piste.track_number = n as i32 + 1;
            piste.duration_ms = if n == 0 {
                DUREE_COURANTE_MS
            } else {
                DUREE_SUIVANTE_MS
            };
            pistes.push(depot.create(&piste).unwrap());
            fichiers.push(f);
        }
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes[..3])
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(faux.sortie()));
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
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(pistes[0]),
                    title: COURANTE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_COURANTE_MS,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 3).await;

        let generation = playback.get_state(zone_id).await.track_generation;
        let mut poll_states = HashMap::new();
        poll_states.insert(zone_id, ZonePollState::new(generation));

        Self {
            poller,
            playback,
            db,
            faux,
            zone_id,
            pistes,
            _fichiers: fichiers,
            poll_states,
            idle: HashMap::new(),
        }
    }

    /// HQPlayer à `position_ms` de la piste en cours, et l'horloge de piste
    /// du sondeur datée en conséquence (garde `played_enough`).
    fn a(&mut self, position_ms: u64) {
        self.faux.a(position_ms as f64 / 1000.0);
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(320));
        ps.peak_position_ms = position_ms;
        ps.last_position_ms = position_ms;
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    async fn ecran(&self) -> (i64, String) {
        let etat = self.playback.get_state(self.zone_id).await;
        (
            etat.queue_position,
            etat.now_playing.map(|np| np.title).unwrap_or_default(),
        )
    }

    /// Les `PlaylistAdd` partis vers HQPlayer.
    fn ajouts(&self) -> Vec<String> {
        self.faux
            .recu()
            .into_iter()
            .filter(|c| c.starts_with("<PlaylistAdd"))
            .collect()
    }

    /// Ce qui interrompt la lecture : vider la playlist, relancer, arrêter.
    fn relances(&self) -> Vec<String> {
        self.faux
            .recu()
            .into_iter()
            .filter(|c| {
                c.contains(r#"clear="1""#) || c.starts_with("<Play ") || c.starts_with("<Stop")
            })
            .collect()
    }

    async fn lire_ensuite(&self, indice_piste: usize, position: i64) {
        let depot = PlayQueueRepo::with_backend(self.db.clone());
        let bilan = depot
            .insert_at_bilan(
                self.zone_id,
                &[QueueInput::Local {
                    track_id: self.pistes[indice_piste],
                }],
                Some(position),
            )
            .unwrap();
        let total = depot.count_all(self.zone_id).unwrap();
        let courante = self.playback.get_state(self.zone_id).await.queue_position;
        let courante = bilan.curseur_apres(courante, total - bilan.inserted() as i64);
        self.playback
            .update_queue_info(self.zone_id, courante, total)
            .await;
    }
}

/// LE FAIT DE BASE de #4851 : dans la fenêtre d'armement, la suivante part
/// dans la playlist de HQPlayer (`clear="0"`), UNE fois ; HQPlayer y passe de
/// lui-même ; l'écran suit — et rien n'a vidé la playlist, relancé ni arrêté
/// la lecture. Avant le correctif : `gapless_skipped_exclusive_output`, aucun
/// ajout, et la fin de piste attendait cinq `Stopped` pour tout relancer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hqplayer_enchaine_par_sa_playlist_sans_relance() {
    let mut banc = Banc::monter().await;

    for s in 0..10u64 {
        banc.a(275_000 + s * 1_000);
        banc.tic().await;
    }
    let ajouts = banc.ajouts();
    assert_eq!(
        ajouts.len(),
        1,
        "#4851 : la suivante doit partir UNE fois dans la playlist de HQPlayer \
         pendant la fenêtre d'armement ; reçu {:?}",
        banc.faux.recu()
    );
    assert!(
        ajouts[0].contains(r#"clear="0""#),
        "l'ajout ne doit pas vider la playlist : {}",
        ajouts[0]
    );
    assert_eq!(banc.faux.playlist().len(), 2, "{:?}", banc.faux.playlist());

    banc.faux.enchaine(DUREE_SUIVANTE_MS as f64 / 1000.0);
    banc.tic().await;

    assert_eq!(
        banc.ecran().await,
        (1, ARMEE.to_string()),
        "HQPlayer est passé sur la suivante : l'écran doit la nommer"
    );
    assert!(
        banc.relances().is_empty(),
        "un enchaînement sans blanc ne vide, ne relance ni n'arrête rien : {:?}",
        banc.relances()
    );
}

/// « Lire ensuite » dans la fenêtre, après l'armement : la suivante déjà
/// posée chez HQPlayer est REMPLACÉE, pas doublée — sinon HQPlayer jouerait
/// la piste armée pendant que l'écran nommerait l'insérée.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lire_ensuite_dans_la_fenetre_remplace_la_suivante_chez_hqplayer() {
    let mut banc = Banc::monter().await;
    banc.a(275_000);
    banc.tic().await;
    let premiere = banc.faux.playlist();
    assert_eq!(premiere.len(), 2, "{:?}", banc.faux.recu());

    banc.lire_ensuite(3, 1).await;
    banc.a(276_000);
    banc.tic().await;

    let playlist = banc.faux.playlist();
    assert_eq!(
        playlist.len(),
        2,
        "la suivante périmée doit sortir de la playlist de HQPlayer : {playlist:?} ; \
         reçu {:?}",
        banc.faux.recu()
    );
    assert_ne!(
        playlist[1], premiere[1],
        "c'est l'insérée qui doit être posée"
    );

    banc.faux.enchaine(DUREE_SUIVANTE_MS as f64 / 1000.0);
    banc.tic().await;
    assert_eq!(banc.ecran().await, (1, INSEREE.to_string()));
    assert!(banc.relances().is_empty(), "{:?}", banc.relances());
}
