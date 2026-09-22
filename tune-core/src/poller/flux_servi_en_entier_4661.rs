//! #4661 — un fichier servi EN ENTIER n'est pas un flux « à sec ».
//!
//! Scène rejouée (Sevy Tabroc, 0.9.161, macOS, darTZeel LHC-208, zone 10) :
//! WAV de 49 596 668 octets pour une piste de 281 160 ms, pré-tiré par le
//! renderer trois minutes avant de la jouer. Le fichier finit d'être servi à
//! 89 s de piste ; le sondeur coupait la zone à `wall_secs=132` en
//! `playback_failure_stopping_zone … peak_pos=0 bytes_sent=50317520
//! consommation="a_sec"` — avec environ 150 s de musique encore dans le
//! tampon du renderer. La file s'arrêtait là.
//!
//! La garde de consommation voulait protéger précisément ce renderer (« le
//! renderer joue mais ne rapporte pas son état — DMP-A10, LHC, Shanling »),
//! mais elle exigeait un compteur qui AUGMENTE : la fin du transfert la
//! désarmait. Désormais, un compteur arrêté parce que le fichier est servi en
//! entier laisse l'horloge trancher : on attend tant que le renderer peut
//! encore jouer, puis on enchaîne comme une fin de piste — on ne coupe plus.
use super::*;
use crate::db::zone_repo::ZoneRepo;
use crate::playback::{NowPlaying, PlayState};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Les chiffres du journal du 21/09/2026.
const DUREE_PISTE_MS: u64 = 281_160;
const TAILLE_FICHIER: u64 = 49_596_668;
const OCTETS_SERVIS: u64 = 50_317_520;
const WALL_A_LA_COUPURE_S: u64 = 132;

// ───────────────────────────── 1. décisions pures ─────────────────────────

#[test]
fn un_compteur_arrete_sur_un_fichier_servi_en_entier_n_est_pas_a_sec() {
    assert_eq!(
        fsm::consommation_flux_au_total(Some(OCTETS_SERVIS), OCTETS_SERVIS, Some(TAILLE_FICHIER)),
        fsm::ConsommationFlux::ServiEnEntier,
        "le fichier est servi en entier : le compteur s'est arrêté faute de matière"
    );
    // Rien de ce qui précédait ne change.
    assert_eq!(
        fsm::consommation_flux_au_total(Some(OCTETS_SERVIS), 0, Some(TAILLE_FICHIER)),
        fsm::ConsommationFlux::Consomme
    );
    assert_eq!(
        fsm::consommation_flux_au_total(Some(1_000), 1_000, Some(TAILLE_FICHIER)),
        fsm::ConsommationFlux::ASec,
        "un flux incomplet qui n'avance plus reste à sec"
    );
    assert_eq!(
        fsm::consommation_flux_au_total(Some(OCTETS_SERVIS), OCTETS_SERVIS, None),
        fsm::ConsommationFlux::ASec,
        "sans taille connue on ne juge pas"
    );
    assert_eq!(
        fsm::consommation_flux_au_total(Some(0), 0, Some(TAILLE_FICHIER)),
        fsm::ConsommationFlux::ASec,
        "zéro octet reste un démarrage mort"
    );
    assert_eq!(
        fsm::consommation_flux_au_total(None, OCTETS_SERVIS, Some(TAILLE_FICHIER)),
        fsm::ConsommationFlux::Inconnue
    );
    assert!(fsm::ConsommationFlux::ServiEnEntier.est_mesuree());
}

#[test]
fn l_horloge_borne_l_attente_du_tampon() {
    assert!(decisions::tampon_du_renderer_peut_encore_jouer(
        WALL_A_LA_COUPURE_S,
        DUREE_PISTE_MS
    ));
    // Jusqu'à la durée + END_MARGIN_MS, pas au-delà.
    assert!(decisions::tampon_du_renderer_peut_encore_jouer(
        283,
        DUREE_PISTE_MS
    ));
    assert!(!decisions::tampon_du_renderer_peut_encore_jouer(
        285,
        DUREE_PISTE_MS
    ));
    assert!(
        !decisions::tampon_du_renderer_peut_encore_jouer(10, 0),
        "durée inconnue : l'horloge ne tranche rien"
    );
}

fn entree(
    wall_elapsed: u64,
    track_duration_ms: u64,
    consommation: fsm::ConsommationFlux,
) -> fsm::StoppedInput {
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
        wall_elapsed,
        track_duration_ms,
        stopped_ticks: STOPPED_FAILURE_THRESHOLD,
        natural_end: false,
        gapless_sent: false,
        realtime: true,
        can_internal_gapless: true,
        consommation,
        dlna_dsd_reached_end: false,
    }
}

#[test]
fn le_modele_attend_puis_enchaine_au_lieu_de_couper() {
    use fsm::{ConsommationFlux::*, StoppedOutcome};
    let attente =
        fsm::classify_stopped(&entree(WALL_A_LA_COUPURE_S, DUREE_PISTE_MS, ServiEnEntier));
    assert_eq!(attente, StoppedOutcome::FailureWaitingServedBuffer);
    assert!(!attente.is_force_stop() && !attente.is_track_end());

    let fin = fsm::classify_stopped(&entree(290, DUREE_PISTE_MS, ServiEnEntier));
    assert_eq!(fin, StoppedOutcome::ServedWholeEndAdvance);
    assert!(fin.is_track_end() && !fin.is_force_stop());

    // Inchangés.
    assert_eq!(
        fsm::classify_stopped(&entree(10, 0, ServiEnEntier)),
        StoppedOutcome::FailureStop,
        "durée inconnue : verdict d'avant"
    );
    assert_eq!(
        fsm::classify_stopped(&entree(WALL_A_LA_COUPURE_S, DUREE_PISTE_MS, ASec)),
        StoppedOutcome::FailureStop
    );
}

// ─────────────────────── 2. le BRANCHEMENT dans tick() ─────────────────────

struct Lhc {
    status: Arc<std::sync::Mutex<OutputStatus>>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl OutputTarget for Lhc {
    fn name(&self) -> &str {
        "LHC-51"
    }
    fn device_id(&self) -> &str {
        "dlna:lhc-51"
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
    stops: Arc<AtomicUsize>,
    zone: i64,
    polls: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    _tmp: tempfile::TempDir,
}

impl Banc {
    /// La zone 10 de Sevy à l'instant où le sondeur commence à compter : le
    /// renderer annonce `Stopped` sans position, le fichier est entièrement
    /// servi, `wall_secs` secondes depuis le début de la piste.
    async fn scene(wall_secs: u64, octets_servis: u64) -> Self {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone = ZoneRepo::with_backend(db.clone())
            .create("DarTZeel LHC 208", Some("dlna"), Some("dlna:lhc-51"))
            .unwrap();
        let status = Arc::new(std::sync::Mutex::new(OutputStatus {
            state: TransportState::Stopped,
            position_ms: 0,
            duration_ms: DUREE_PISTE_MS,
            realtime: true,
            ..Default::default()
        }));
        let stops = Arc::new(AtomicUsize::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(Lhc {
            status,
            stops: stops.clone(),
        }));
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        let streamer = Arc::new(crate::http::streamer::AudioStreamer::new(0));
        let tmp = tempfile::TempDir::new().unwrap();
        let fichier = tmp.path().join("mirror-mirror.wav");
        std::fs::write(&fichier, b"RIFF").unwrap();
        let sid = streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    file_size: Some(TAILLE_FICHIER),
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
                .store(octets_servis, Ordering::Relaxed);
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
                    title: "Mirror, Mirror".into(),
                    artist_name: Some("Mitch Malloy".into()),
                    source: "local".into(),
                    stream_id: Some(sid),
                    duration_ms: DUREE_PISTE_MS as i64,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone, 14, 40).await;
        let mut ps = ZonePollState::new(playback.get_state(zone).await.track_generation);
        ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(wall_secs));
        ps.track_loaded_at = Instant::now() - Duration::from_secs(wall_secs);
        Self {
            poller,
            stops,
            zone,
            polls: HashMap::from([(zone, ps)]),
            idle: HashMap::new(),
            _tmp: tmp,
        }
    }

    async fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.poller
                .tick(&mut self.polls, &mut self.idle, &Instant::now())
                .await;
        }
    }

    async fn etat(&self) -> PlayState {
        self.poller.playback.get_state(self.zone).await.state
    }
}

/// LE témoin du ticket : la scène exacte du 21/09. Avant le correctif, le
/// 31ᵉ tour `Stopped` coupait la zone (`FailureStop`) ; la file s'arrêtait.
#[tokio::test]
async fn la_zone_n_est_plus_coupee_quand_le_fichier_est_servi_en_entier() {
    let mut b = Banc::scene(WALL_A_LA_COUPURE_S, OCTETS_SERVIS).await;
    b.ticks(STOPPED_FAILURE_THRESHOLD as usize + 10).await;
    assert_eq!(
        b.etat().await,
        PlayState::Playing,
        "fichier servi en entier, ~150 s de musique encore dans le tampon : la zone ne doit pas être coupée"
    );
    assert_eq!(
        b.stops.load(Ordering::SeqCst),
        0,
        "aucun Stop envoyé au renderer"
    );
}

/// Contre-garde : un flux INCOMPLET dont le compteur n'avance plus est
/// toujours un flux à sec, et la zone est toujours coupée comme avant.
#[tokio::test]
async fn un_flux_incomplet_a_sec_est_toujours_coupe() {
    let mut b = Banc::scene(WALL_A_LA_COUPURE_S, TAILLE_FICHIER / 2).await;
    b.ticks(STOPPED_FAILURE_THRESHOLD as usize + 10).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "un flux incomplet à sec doit toujours couper la zone"
    );
}
