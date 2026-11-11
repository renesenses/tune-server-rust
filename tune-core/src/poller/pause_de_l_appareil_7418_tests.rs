//! #7418 — une pause faite sur l'appareil DLNA (télécommande WiiM, autre
//! entrée) met la zone en pause, et l'arrêt qui suit ne relance rien.
//!
//! Le banc est celui de production : base SQLite en mémoire avec ses
//! migrations, orchestrateur, gestionnaire de lecture et `tick()` du sondeur.
//! Seul l'appareil est simulé : il rapporte l'état qu'on lui donne, comme un
//! WiiM interrogé par `GetTransportInfo`, et compte ce qu'on lui envoie.

use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::playback::{NowPlaying, PlayState, PlaybackManager, RepeatMode};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;

const DEVICE_ID: &str = "dlna:wiim-ultra-7418";
const NOTRE_FLUX: &str = "flux7418";
const POSITION_DE_PAUSE_MS: u64 = 10_000;

/// Ce que l'appareil a reçu de Tune.
#[derive(Default)]
struct Recu {
    lectures: AtomicU32,
    autres: AtomicU32,
    /// Sous-compte de `lectures` : les nouveaux flux (`play_media`).
    nouveaux_flux: AtomicU32,
    /// Sous-compte de `autres` : les `Stop`.
    arrets: AtomicU32,
}

/// Un WiiM vu par DLNA : l'état et l'URI qu'il rapporte sont fixés par le
/// banc, la position est figée (pause, puis arrêt).
struct Wiim {
    etat: Arc<std::sync::Mutex<(TransportState, Option<String>)>>,
    recu: Arc<Recu>,
}

#[async_trait::async_trait]
impl OutputTarget for Wiim {
    fn name(&self) -> &str {
        "WiiM Ultra"
    }
    fn device_id(&self) -> &str {
        DEVICE_ID
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    /// Comme un vrai renderer DLNA : pause, reprise, arrêt et `Seek` admis.
    fn capabilities(&self) -> crate::outputs::OutputCapabilities {
        crate::outputs::OutputCapabilities::v1(true, true, true, true, true, true)
    }
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        self.recu.lectures.fetch_add(1, Ordering::SeqCst);
        self.recu.nouveaux_flux.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        self.recu.autres.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        self.recu.lectures.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        self.recu.autres.fetch_add(1, Ordering::SeqCst);
        self.recu.arrets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        self.recu.autres.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        let (state, current_uri) = self.etat.lock().unwrap().clone();
        Ok(OutputStatus {
            state,
            position_ms: POSITION_DE_PAUSE_MS,
            duration_ms: 240_000,
            volume: 0.5,
            current_uri,
            realtime: true,
            ..Default::default()
        })
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
    etat: Arc<std::sync::Mutex<(TransportState, Option<String>)>>,
    recu: Arc<Recu>,
    poll_states: HashMap<i64, ZonePollState>,
    idle_backoff: HashMap<i64, IdlePollBackoff>,
    streamer: Arc<AudioStreamer>,
    /// La session de flux que l'appareil joue (`NOTRE_FLUX` tant qu'aucune
    /// vraie session n'est ouverte, voir [`Banc::ouvrir_une_vraie_session`]).
    flux: String,
    fichier: String,
    _fichiers: tempfile::TempDir,
}

impl Banc {
    /// Une zone qui joue une piste locale de la file sur le WiiM.
    async fn monter(repetition: RepeatMode) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(DEVICE_ID))
            .unwrap();

        // Une vraie piste en base et sur disque : une relance du sondeur
        // (répétition, suivante) irait jusqu'à l'appareil.
        let fichiers = tempfile::tempdir().unwrap();
        let fichier = fichiers.path().join("potion.wav");
        let mut wav =
            crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(240_000)).to_vec();
        wav.resize(wav.len() + 44100 * 4, 0);
        std::fs::write(&fichier, wav).unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Rob');
            INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Salon', 1, 2026);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, duration_ms)
            VALUES (1, 'Potion', 1, 1, ?, 'wav', 240000)",
            &[&fichier.to_str().unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO queue_items (zone_id, position, track_id, source)
            VALUES (?, 0, 1, 'local')",
            &[&zone_id],
        )
        .unwrap();

        let etat = Arc::new(std::sync::Mutex::new((
            TransportState::Playing,
            Some(format!("http://192.168.1.10:8888/stream/{NOTRE_FLUX}")),
        )));
        let recu = Arc::new(Recu::default());
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(Wiim {
            etat: etat.clone(),
            recu: recu.clone(),
        }));

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
                    track_id: Some(1),
                    title: "Potion".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    stream_id: Some(NOTRE_FLUX.into()),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 1).await;
        playback.set_repeat(zone_id, repetition).await;
        ZoneRepo::with_backend(db.clone())
            .save_play_state(zone_id, "playing")
            .unwrap();

        Self {
            poller,
            playback,
            db,
            zone_id,
            etat,
            recu,
            poll_states: HashMap::new(),
            idle_backoff: HashMap::new(),
            streamer,
            flux: NOTRE_FLUX.into(),
            fichier: fichier.to_str().unwrap().to_string(),
            _fichiers: fichiers,
        }
    }

    fn appareil(&self, state: TransportState, uri: Option<&str>) {
        *self.etat.lock().unwrap() = (state, uri.map(str::to_string));
    }

    fn notre_uri() -> String {
        format!("http://192.168.1.10:8888/stream/{NOTRE_FLUX}")
    }

    fn uri_du_flux(&self) -> String {
        format!("http://192.168.1.10:8888/stream/{}", self.flux)
    }

    /// La zone joue une session de flux VIVANTE : sans elle, toute reprise
    /// passe déjà par un nouveau flux (session morte, #2512), et le banc ne
    /// distinguerait pas la reprise sur place.
    async fn ouvrir_une_vraie_session(&mut self) {
        let sid = self
            .streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 44100,
                    bit_depth: 16,
                    channels: 2,
                    duration_ms: Some(240_000),
                    ..Default::default()
                },
                self.fichier.clone(),
                false,
            )
            .await;
        self.flux = sid.clone();
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    track_id: Some(1),
                    title: "Potion".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    stream_id: Some(sid),
                    ..Default::default()
                },
            )
            .await;
        self.playback.update_queue_info(self.zone_id, 0, 1).await;
    }

    async fn tours(&mut self, n: usize) {
        for _ in 0..n {
            self.poller
                .tick(
                    &mut self.poll_states,
                    &mut self.idle_backoff,
                    &Instant::now(),
                )
                .await;
        }
    }

    async fn etat_de_zone(&self) -> PlayState {
        self.playback.get_state(self.zone_id).await.state
    }

    /// Lecture, puis pause à la télécommande tenue assez longtemps.
    async fn pause_a_la_telecommande(&mut self) {
        let uri = self.uri_du_flux();
        self.appareil(TransportState::Playing, Some(&uri));
        self.tours(2).await;
        self.appareil(TransportState::Paused, Some(&uri));
        self.tours(super::pause_de_l_appareil_7418::TOURS_DE_PAUSE_DE_L_APPAREIL as usize)
            .await;
    }
}

/// LE CONSTAT : l'appareil est en pause (télécommande WiiM), la zone ne doit
/// plus se dire en lecture. Elle passe `paused`, en mémoire et en base, sans
/// qu'aucune commande ne parte vers l'appareil.
#[tokio::test]
async fn une_pause_a_la_telecommande_met_la_zone_en_pause_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;

    banc.pause_a_la_telecommande().await;

    assert_eq!(
        banc.etat_de_zone().await,
        PlayState::Paused,
        "#7418 : l'appareil est en pause depuis cinq tours, la zone restait « playing »"
    );
    assert_eq!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("paused"),
        "l'état enregistré doit être celui d'une pause"
    );
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(
        banc.recu.autres.load(Ordering::SeqCst),
        0,
        "l'appareil est déjà en pause : la zone le suit sans rien lui renvoyer"
    );
}

/// LA RELANCE DU FIL 2531 : pause à la télécommande, puis l'appareil passe
/// `Stopped` (autre entrée, téléviseur éteint). Répétition active, le bras
/// `Stopped` d'une zone « en lecture » prenait cet arrêt pour une fin de piste
/// et renvoyait une lecture à l'appareil : la musique repartait seule.
#[tokio::test]
async fn apres_la_pause_de_l_appareil_son_arret_ne_relance_rien_7418() {
    let mut banc = Banc::monter(RepeatMode::One).await;

    banc.pause_a_la_telecommande().await;
    banc.appareil(TransportState::Stopped, None);
    banc.tours(40).await;

    assert_eq!(
        banc.recu.lectures.load(Ordering::SeqCst),
        0,
        "#7418 : après une pause faite sur l'appareil, son arrêt ne doit relancer aucune lecture"
    );
    assert_ne!(
        banc.etat_de_zone().await,
        PlayState::Playing,
        "la zone ne doit pas se remettre en lecture toute seule"
    );
}

/// TÉMOIN : un passage bref par `PAUSED_PLAYBACK` (chargement, reprise lente
/// d'un renderer) n'emmène pas la zone.
#[tokio::test]
async fn une_pause_breve_de_l_appareil_laisse_la_zone_en_lecture_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    let uri = Banc::notre_uri();

    banc.appareil(TransportState::Playing, Some(&uri));
    banc.tours(2).await;
    banc.appareil(TransportState::Paused, Some(&uri));
    banc.tours(super::pause_de_l_appareil_7418::TOURS_DE_PAUSE_DE_L_APPAREIL as usize - 1)
        .await;
    banc.appareil(TransportState::Playing, Some(&uri));
    banc.tours(1).await;

    assert_eq!(banc.etat_de_zone().await, PlayState::Playing);
    // La série repart de zéro : quatre tours de plus en pause ne suffisent pas.
    banc.appareil(TransportState::Paused, Some(&uri));
    banc.tours(super::pause_de_l_appareil_7418::TOURS_DE_PAUSE_DE_L_APPAREIL as usize - 1)
        .await;
    assert_eq!(banc.etat_de_zone().await, PlayState::Playing);
}

/// Lecture à la télécommande après la pause : l'appareil rejoue NOTRE flux,
/// la zone reprend avec lui, sans commande renvoyée.
#[tokio::test]
async fn la_lecture_a_la_telecommande_fait_reprendre_la_zone_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.pause_a_la_telecommande().await;
    assert_eq!(banc.etat_de_zone().await, PlayState::Paused);

    let uri = Banc::notre_uri();
    banc.appareil(TransportState::Playing, Some(&uri));
    // La zone en pause est sondée à la cadence de repos.
    banc.tours(8).await;

    assert_eq!(
        banc.etat_de_zone().await,
        PlayState::Playing,
        "l'appareil rejoue notre flux : la zone doit reprendre avec lui"
    );
    assert_eq!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("playing")
    );
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 0);
}

/// Une AUTRE application (Emby) joue sur l'appareil après la pause : la zone
/// ne reprend pas, rien ne part. Depuis rc12 elle s'ARRÊTE (l'appareil est
/// passé à autre chose), sans couper Emby.
#[tokio::test]
async fn une_autre_application_sur_l_appareil_ne_fait_pas_reprendre_la_zone_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.pause_a_la_telecommande().await;

    banc.appareil(
        TransportState::Playing,
        Some("http://192.168.1.20:8096/Audio/abc/stream.flac"),
    );
    banc.tours(8).await;

    assert_ne!(banc.etat_de_zone().await, PlayState::Playing);
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(
        banc.recu.arrets.load(Ordering::SeqCst),
        0,
        "la musique d'Emby ne doit pas être coupée"
    );
}

#[test]
fn seule_une_sortie_dlna_hors_grace_suit_la_pause_de_son_appareil_7418() {
    use super::pause_de_l_appareil_7418::{TOURS_DE_PAUSE_DE_L_APPAREIL as N, *};
    assert!(pause_de_l_appareil_etablie("dlna", N, false));
    assert!(!pause_de_l_appareil_etablie("dlna", N - 1, false));
    assert!(!pause_de_l_appareil_etablie("dlna", N, true));
    assert!(!pause_de_l_appareil_etablie("local", N, false));
    assert!(!pause_de_l_appareil_etablie("oaat", N, false));
}

// ── rc12 : la pause de l'appareil devient un arrêt complet ──────────────────

impl Banc {
    /// Fait comme si la zone avait suivi la pause de l'appareil il y a
    /// `duree` : l'horloge murale ne se simule pas autrement.
    fn vieillir_la_pause(&self, duree: std::time::Duration) {
        let mut zones = self.poller.pauses_de_l_appareil.lock().unwrap();
        let suivi = zones
            .get_mut(&self.zone_id)
            .expect("la zone doit être suivie comme mise en pause par son appareil");
        suivi.depuis = Instant::now() - duree;
    }

    async fn reprise_armee(&self) -> Option<i64> {
        self.playback.pending_resume_ms(self.zone_id).await
    }

    fn etat_enregistre(&self) -> Option<String> {
        ZoneRepo::with_backend(self.db.clone()).get_last_play_state(self.zone_id)
    }
}

/// CHANGEMENT D'ENTRÉE (fil 2531) : pause à la télécommande, puis l'appareil
/// s'arrête (autre entrée, téléviseur). La zone ne reste plus « en pause »
/// sur un appareil qui n'a plus notre piste : elle s'ARRÊTE, sans commande
/// vers l'appareil, piste et position armées pour « Lecture » dans Tune.
#[tokio::test]
async fn l_appareil_passe_a_autre_chose_la_zone_s_arrete_sans_commande_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.pause_a_la_telecommande().await;
    assert_eq!(banc.etat_de_zone().await, PlayState::Paused);

    banc.appareil(TransportState::Stopped, None);
    banc.tours(40).await;

    assert_eq!(
        banc.etat_de_zone().await,
        PlayState::Stopped,
        "#7418 : l'appareil a quitté notre flux, la zone restait « en pause » et « Lecture » n'y renvoyait qu'un Play sur place"
    );
    assert_eq!(banc.etat_enregistre().as_deref(), Some("stopped"));
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(
        banc.recu.autres.load(Ordering::SeqCst),
        0,
        "l'appareil est déjà ailleurs : rien ne lui est envoyé"
    );
    let np = banc
        .playback
        .get_state(banc.zone_id)
        .await
        .now_playing
        .expect("la piste reste en mémoire pour « Lecture »");
    assert_eq!(np.track_id, Some(1));
    assert_eq!(
        banc.reprise_armee().await,
        Some(POSITION_DE_PAUSE_MS as i64),
        "« Lecture » dans Tune doit repartir de la position de la pause"
    );
}

/// PAUSE TENUE (fil 2531, « 10 à 15 minutes après ») : l'appareil garde
/// notre flux en pause. Tant que dure le délai, rien ne bouge (la télécommande
/// peut reprendre) ; passé le délai, Tune arrête l'appareil — il ne pourra
/// plus reprendre seul — et la zone.
#[tokio::test]
async fn une_pause_tenue_sur_notre_flux_devient_un_arret_complet_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.pause_a_la_telecommande().await;

    banc.tours(30).await;
    assert_eq!(
        banc.etat_de_zone().await,
        PlayState::Paused,
        "avant le délai, la pause reste une pause"
    );
    assert_eq!(banc.recu.autres.load(Ordering::SeqCst), 0);

    banc.vieillir_la_pause(super::pause_de_l_appareil_7418::DELAI_AVANT_L_ARRET_DE_L_APPAREIL);
    banc.tours(10).await;

    assert_eq!(
        banc.etat_de_zone().await,
        PlayState::Stopped,
        "#7418 : passé le délai, la pause de l'appareil doit devenir un arrêt complet"
    );
    assert_eq!(
        banc.recu.arrets.load(Ordering::SeqCst),
        1,
        "#7418 : l'appareil garde notre flux en pause, un Stop doit l'empêcher de le reprendre seul"
    );
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(banc.etat_enregistre().as_deref(), Some("stopped"));
    assert_eq!(
        banc.reprise_armee().await,
        Some(POSITION_DE_PAUSE_MS as i64),
        "#7418 : « Lecture » dans Tune doit repartir de la position de la pause"
    );
}

/// « LECTURE » DANS TUNE pendant la pause de l'appareil : un nouveau flux
/// à la position, jamais un `Play` sur place vers un appareil qui a pu
/// changer d'entrée.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_lecture_dans_tune_apres_la_pause_de_l_appareil_retablit_notre_flux_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.ouvrir_une_vraie_session().await;
    banc.pause_a_la_telecommande().await;
    assert_eq!(banc.etat_de_zone().await, PlayState::Paused);

    banc.poller
        .orchestrator
        .resume(banc.zone_id, Some(DEVICE_ID))
        .await
        .expect("la reprise depuis Tune doit aboutir");

    assert_eq!(
        banc.recu.nouveaux_flux.load(Ordering::SeqCst),
        1,
        "#7418 : la reprise depuis Tune doit rétablir notre flux, pas envoyer un Play sur place"
    );
    assert_eq!(
        banc.recu.lectures.load(Ordering::SeqCst),
        1,
        "un seul lancement, aucun Play sur place en plus"
    );
    assert_eq!(banc.etat_de_zone().await, PlayState::Playing);
}

/// TÉMOIN : une pause demandée DANS TUNE garde la reprise sur place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_pause_de_tune_garde_la_reprise_sur_place_7418() {
    let mut banc = Banc::monter(RepeatMode::Off).await;
    banc.ouvrir_une_vraie_session().await;
    banc.poller
        .orchestrator
        .pause(banc.zone_id, Some(DEVICE_ID))
        .await
        .unwrap();
    banc.poller
        .orchestrator
        .resume(banc.zone_id, Some(DEVICE_ID))
        .await
        .unwrap();
    assert_eq!(banc.recu.nouveaux_flux.load(Ordering::SeqCst), 0);
    assert_eq!(banc.recu.lectures.load(Ordering::SeqCst), 1);
}

#[test]
fn la_suite_d_une_pause_de_l_appareil_7418() {
    use super::decisions::RELEVES_AVANT_DE_CEDER as R;
    use super::pause_de_l_appareil_7418::{
        DELAI_AVANT_L_ARRET_DE_L_APPAREIL as D, Releve, Suite, releve, suite_de_la_pause,
    };
    use std::time::Duration;
    let z = Duration::ZERO;
    assert_eq!(releve(TransportState::Playing, true), Releve::JoueLeNotre);
    assert_eq!(
        releve(TransportState::Paused, true),
        Releve::EnPauseSurLeNotre
    );
    assert_eq!(releve(TransportState::Paused, false), Releve::Ailleurs);
    assert_eq!(releve(TransportState::Playing, false), Releve::Ailleurs);
    assert_eq!(releve(TransportState::Stopped, true), Releve::Ailleurs);
    assert_eq!(
        suite_de_la_pause(Releve::JoueLeNotre, 0, D),
        Suite::Reprendre
    );
    assert_eq!(
        suite_de_la_pause(Releve::Ailleurs, R - 1, D),
        Suite::Attendre
    );
    assert_eq!(
        suite_de_la_pause(Releve::Ailleurs, R, z),
        Suite::ArreterSansCommande
    );
    assert_eq!(
        suite_de_la_pause(Releve::EnPauseSurLeNotre, 0, D - Duration::from_secs(1)),
        Suite::Attendre
    );
    assert_eq!(
        suite_de_la_pause(Releve::EnPauseSurLeNotre, 0, D),
        Suite::ArreterLAppareil
    );
}
