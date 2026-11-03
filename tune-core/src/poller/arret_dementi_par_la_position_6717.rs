//! #6717 — un renderer qui se dit `Stopped` mais dont la position AVANCE
//! joue : la zone n'est pas coupée sur « flux à sec ».
//!
//! ## La scène (Windows rc6, renderer `GStreamer souphttpsrc 1.14.4`)
//!
//! | coupure | `peak_pos` | `wall_secs` | `arret_secs` | `avance_ms` |
//! |---|---|---|---|---|
//! | 13:03:18 | 130 000 | 129 | 32 | 30 550 |
//! | 13:08:12 | 159 000 | 161 | 36 | 27 575 |
//!
//! Aux deux coupures, la position atteinte égale le temps écoulé depuis le
//! début de la piste, alors que la série `Stopped` dure depuis plus de 30 s.
//! Si le renderer s'était arrêté au début de la série, son pic serait resté
//! vers 97 s et 125 s. Il a donc continué de jouer pendant tout l'« arrêt » ;
//! l'avance d'audio d'environ 30 s est le tampon plein d'un renderer qui
//! joue, et la socket « à sec » une livraison à contre-pression.
//!
//! La seconde coupure tombe 0,25 s après un gel de l'exécuteur de 20 s : la
//! rafale de tours qui suit le gel a seulement avancé l'échéance. Le gel
//! n'est pas la cause, la série « arrêtée » avait commencé 6 s avant lui.
//!
//! ## La règle posée ici
//!
//! Dans le bras `Stopped`, une position qui dépasse le pic d'avant le tour
//! dément l'arrêt : le tour ne compte pas, la série en cours est effacée
//! ([`fsm::arret_dementi_par_la_position`]). Un renderer réellement arrêté
//! (position figée) est toujours coupé comme avant : c'est la contre-garde.
use super::*;
use crate::db::zone_repo::ZoneRepo;
use crate::playback::{NowPlaying, PlayState};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Les chiffres de la première coupure du journal (zone 3).
const DUREE_PISTE_MS: u64 = 259_000;
const TAILLE_FLUX: u64 = 30_765_396;
/// 62 % servis : le fichier n'est PAS chez le renderer en entier, l'horloge
/// de piste de #4661 n'épargne donc rien.
const OCTETS_SERVIS: u64 = 19_070_976;
/// Début de la série `Stopped` : 129 s de piste moins 32 s d'arrêt.
const POSITION_AU_DEBUT_DE_L_ARRET_MS: u64 = 97_000;
const ARRET_A_LA_COUPURE_S: u64 = 32;

// ───────────────────────── 1. les décisions pures ─────────────────────────

#[test]
fn une_position_qui_depasse_le_pic_dement_l_arret() {
    assert!(fsm::arret_dementi_par_la_position(129_000, 130_000));
    assert!(
        !fsm::arret_dementi_par_la_position(130_000, 130_000),
        "position figée : le renderer est arrêté"
    );
    assert!(
        !fsm::arret_dementi_par_la_position(130_000, 0),
        "position remise à zéro : rien ne joue"
    );
    assert!(
        !fsm::arret_dementi_par_la_position(130_000, 129_000),
        "une position qui recule sous le pic ne gagne aucun terrain"
    );
}

fn entree(position_avance: bool, stopped_ticks: u8) -> fsm::StoppedInput {
    fsm::StoppedInput {
        tune_is_playing: true,
        tune_has_track: true,
        in_seek_grace: false,
        in_track_load_grace: false,
        gapless_cooldown: 0,
        in_gapless_guard: false,
        played_enough: false,
        gapless_advance_pending: false,
        gapless_stuck_ticks: 0,
        ended_naturally: false,
        wall_elapsed: 129,
        track_duration_ms: DUREE_PISTE_MS,
        stopped_ticks,
        natural_end: false,
        gapless_sent: false,
        realtime: true,
        can_internal_gapless: true,
        consommation: fsm::ConsommationFlux::ASec,
        avance_audio_couvre_l_arret: false,
        horloge_de_piste_couvre_l_arret: false,
        dlna_dsd_reached_end: false,
        position_avance,
    }
}

#[test]
fn le_classeur_ne_coupe_pas_un_renderer_dont_la_position_avance() {
    assert_eq!(
        fsm::classify_stopped(&entree(false, STOPPED_FAILURE_THRESHOLD)),
        fsm::StoppedOutcome::FailureStop,
        "contre-garde : même tour, position figée, la zone est coupée"
    );
    assert_eq!(
        fsm::classify_stopped(&entree(true, STOPPED_FAILURE_THRESHOLD)),
        fsm::StoppedOutcome::SuppressPositionAdvancing
    );
    // Une fin de piste annoncée par la sortie garde la priorité.
    let fin = fsm::StoppedInput {
        ended_naturally: true,
        played_enough: true,
        ..entree(true, 0)
    };
    assert_eq!(
        fsm::classify_stopped(&fin),
        fsm::StoppedOutcome::LocalEndedNaturally
    );
}

// ───────────────────── 2. le BRANCHEMENT dans tick() ──────────────────────

struct Gstreamer {
    status: Arc<std::sync::Mutex<OutputStatus>>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl OutputTarget for Gstreamer {
    fn name(&self) -> &str {
        "Zone 2"
    }
    fn device_id(&self) -> &str {
        "dlna:gstreamer-6717"
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    fn supports_internal_gapless(&self) -> bool {
        true
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
    _scratch: crate::test_scratch::ScratchDir,
}

impl Banc {
    /// La zone 3 du journal : la piste joue depuis 97 s, le renderer a
    /// atteint 97 s, et il annonce désormais `Stopped`. 62 % du flux sont
    /// servis et la socket ne bouge plus.
    async fn scene() -> Self {
        let wall_secs = POSITION_AU_DEBUT_DE_L_ARRET_MS / 1_000;
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Zone 2", Some("dlna"), Some("dlna:gstreamer-6717"))
            .unwrap();
        let status = Arc::new(std::sync::Mutex::new(OutputStatus {
            state: TransportState::Stopped,
            position_ms: POSITION_AU_DEBUT_DE_L_ARRET_MS,
            duration_ms: DUREE_PISTE_MS,
            realtime: true,
            ..Default::default()
        }));
        let stops = Arc::new(AtomicUsize::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(Gstreamer {
            status: status.clone(),
            stops: stops.clone(),
        }));
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        let streamer = Arc::new(crate::http::streamer::AudioStreamer::new(0));
        let scratch = crate::test_scratch::scratch_dir("tune-arret-dementi-6717");
        let fichier = scratch.join("piste.flac");
        std::fs::write(&fichier, b"fLaC").unwrap();
        let sid = streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    file_size: Some(TAILLE_FLUX),
                    ..Default::default()
                },
                fichier.to_string_lossy().into_owned(),
                false,
            )
            .await;
        {
            let sessions = streamer.sessions_state();
            let sessions = sessions.lock().await;
            sessions
                .get(&sid)
                .expect("la session vient d'être créée")
                .bytes_sent
                .store(OCTETS_SERVIS, Ordering::Relaxed);
        }
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            streamer,
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
                    title: "Piste 7".into(),
                    artist_name: Some("Testeur du fil 2365".into()),
                    source: "local".into(),
                    stream_id: Some(sid),
                    duration_ms: DUREE_PISTE_MS as i64,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone, 7, 12).await;
        let mut ps = ZonePollState::new(playback.get_state(zone).await.track_generation);
        ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(wall_secs));
        ps.track_loaded_at = Instant::now() - Duration::from_secs(wall_secs);
        ps.peak_position_ms = POSITION_AU_DEBUT_DE_L_ARRET_MS;
        ps.last_position_ms = POSITION_AU_DEBUT_DE_L_ARRET_MS;
        Self {
            poller,
            status,
            stops,
            zone,
            polls: HashMap::from([(zone, ps)]),
            idle: HashMap::new(),
            _scratch: scratch,
        }
    }

    /// Un tour de sondeur ; le renderer, toujours `Stopped`, rapporte la
    /// position avancée de `pas_ms` depuis le tour d'avant.
    ///
    /// L'horloge de la piste avance d'autant : un renderer qui joue gagne
    /// une seconde de position par seconde écoulée. Sans cela, la position
    /// distance l'horloge du banc de plus de 15 s au seizième tour, et la
    /// garde `stale_start_position` écarte tous les tours suivants : le
    /// seuil de coupure n'est jamais atteint, correctif ou non.
    async fn tour(&mut self, pas_ms: u64) {
        self.status.lock().unwrap().position_ms += pas_ms;
        // Une zone coupée n'est plus suivie : rien à faire avancer.
        if let Some(ps) = self.polls.get_mut(&self.zone) {
            let pas = Duration::from_millis(pas_ms);
            ps.track_started_at = ps.track_started_at.and_then(|t| t.checked_sub(pas));
            ps.track_loaded_at -= pas;
        }
        self.poller
            .tick(&mut self.polls, &mut self.idle, &Instant::now())
            .await;
    }

    async fn tours(&mut self, count: usize, pas_ms: u64) {
        for _ in 0..count {
            self.tour(pas_ms).await;
        }
    }

    /// Reculer l'horloge de la série d'arrêts en cours, s'il y en a une :
    /// trente tours de banc prennent quelques millisecondes, le plancher de
    /// #4480 en exige trente secondes.
    fn vieillir_l_arret(&mut self, secs: u64) {
        let ps = self.polls.get_mut(&self.zone).expect("la zone du banc");
        if ps.premier_arret_a.is_some() {
            ps.premier_arret_a = Instant::now().checked_sub(Duration::from_secs(secs));
        }
    }

    async fn etat(&self) -> PlayState {
        self.poller.playback.get_state(self.zone).await.state
    }
}

/// POLARITÉ 1 — la première coupure du journal. Le renderer annonce
/// `Stopped` à chaque tour, et sa position avance d'une seconde par tour :
/// au bout de 32 s d'« arrêt », il est à 130 s, comme dans le journal. La
/// zone ne doit pas être coupée.
#[tokio::test]
async fn un_renderer_arrete_dont_la_position_avance_garde_sa_zone() {
    let mut b = Banc::scene().await;
    b.tours(STOPPED_FAILURE_THRESHOLD as usize + 1, 1_000).await;
    b.vieillir_l_arret(ARRET_A_LA_COUPURE_S);
    b.tours(2, 1_000).await;
    assert_eq!(
        b.status.lock().unwrap().position_ms,
        130_000,
        "le banc rejoue la position de la coupure du journal"
    );
    assert_eq!(
        b.etat().await,
        PlayState::Playing,
        "le renderer se dit arrêté mais sa position avance : il joue, la zone \
         ne doit pas être coupée sur « flux à sec » (#6717)"
    );
    assert_eq!(
        b.stops.load(Ordering::SeqCst),
        0,
        "aucun Stop envoyé au renderer"
    );
}

/// POLARITÉ 1 bis — la seconde coupure : la série `Stopped` commence 6 s
/// avant un gel de l'exécuteur, le gel dure 30 s, et la rafale de tours qui
/// le suit lit la même position à chaque tour (le renderer la donne à la
/// seconde, et la rafale tient en quelques millisecondes).
#[tokio::test]
async fn la_rafale_apres_un_gel_ne_coupe_pas_un_renderer_qui_joue() {
    let mut b = Banc::scene().await;
    b.tours(6, 1_000).await;
    b.vieillir_l_arret(36);
    // Premier tour après le gel : 30 s de musique jouées pendant le gel.
    b.tour(30_000).await;
    // La rafale : trente tours au même échantillon.
    b.tours(STOPPED_FAILURE_THRESHOLD as usize + 2, 0).await;
    assert_eq!(
        b.etat().await,
        PlayState::Playing,
        "la position a gagné 30 s pendant le gel : la série d'arrêts d'avant \
         le gel est démentie, la rafale ne doit pas couper la zone (#6717)"
    );
    assert_eq!(b.stops.load(Ordering::SeqCst), 0);
}

/// POLARITÉ 2 — LA CONTRE-GARDE. Même scène, même arrêt, même flux à 62 % :
/// seule la position est FIGÉE. Le renderer est réellement arrêté, la zone
/// doit être coupée comme avant. C'est aussi la preuve que le banc atteint
/// bien le bras qui coupe.
#[tokio::test]
async fn un_renderer_arrete_a_position_figee_est_toujours_coupe() {
    let mut b = Banc::scene().await;
    b.tours(STOPPED_FAILURE_THRESHOLD as usize + 1, 0).await;
    assert!(
        b.polls[&b.zone].stopped_ticks >= STOPPED_FAILURE_THRESHOLD,
        "le banc n'a pas atteint le seuil en TOURS : rien ne serait jugé"
    );
    b.vieillir_l_arret(ARRET_A_LA_COUPURE_S);
    b.tours(2, 0).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "position figée, flux à 62 %, socket à sec depuis 32 s : la zone doit \
         toujours être coupée"
    );
}
