//! #4661 (fil forum 1912) — le démarrage mort d'un renderer qui SONDE avant
//! de lire.
//!
//! ## La scène (Sevy Tabroc, 0.9.163, darTZeel LHC-208, zone 10, 24/09/2026)
//!
//! ```text
//! 14:27:27.348  stream_request range="-"        agent="player/100" format=wav
//! 14:27:27.392  service_fichier_termine octets=851968 demande=52510796 elapsed_ms=43 complet=false fin="consommateur_parti"
//! 14:27:27.396  stream_request range="bytes=44-" agent="player/100" format=wav
//! 14:27:27.442  service_fichier_termine octets=720896 demande=52510752 elapsed_ms=44 complet=false fin="consommateur_parti"
//! 14:27:27.529  gapless_position_reset_detected zone_id=10 prev_pos=293000 new_pos=0
//! 14:27:27.535  avance_gapless_flux_adopte zone_id=10 position=11
//! 14:28:00.533  WARN playback_failure_stopping_zone peak_pos=0 track_dur=297680 wall_secs=33
//!               bytes_sent=1572864 consommation="a_sec" arret_ticks=33 arret_secs=31
//!               audio_servi_ms=8916 avance_ms=8916
//! 14:28:00.700  dlna_stop device=LHC-55
//! ```
//!
//! Le renderer a ouvert la piste suivante comme toutes les autres (requête
//! sans `Range` lâchée, puis `bytes=44-`), mais a refermé la seconde au bout
//! de 44 ms au lieu de la tirer jusqu'au bout. Il est resté `Stopped` à la
//! position 0. Le flux n'était ni lent ni en retard : 720 896 octets en
//! 44 ms, fichier pré-transcodé depuis 26 s.
//!
//! ## Pourquoi la zone était coupée
//!
//! Le bras du seuil d'échec (`tick.rs`) juge juste : socket à sec, 8,9 s
//! d'audio servie, 31 s d'arrêt — la famine est établie, et le fichier n'est
//! pas servi en entier, l'horloge de piste ne couvre rien. Mais des deux
//! sauvetages qui suivent la coupure, aucun ne s'armait :
//!
//! - la relance du démarrage mort (#2394) exigeait **zéro octet** servi —
//!   ce que le LHC, qui sonde toujours ~700 Kio, ne rend jamais ;
//! - la reprise à la position atteinte (#4645) exige une piste qui **a
//!   joué** (`position > 0`), et renvoie explicitement la position 0 au
//!   démarrage mort.
//!
//! Le cas tombait entre les deux : zone coupée, file arrêtée.
//!
//! ## Ce que ce banc garde
//!
//! Le renderer factice imite le LHC tel que le journal le montre : `Stopped`,
//! position 0, sur un flux WAV dont la session porte les octets des deux
//! requêtes abandonnées. Le contrat HTTP lui-même (longueur annoncée,
//! `Accept-Ranges`, `206` sur `bytes=44-`, octets comptés malgré les deux
//! abandons) est éprouvé à part, contre le vrai routeur, dans
//! `tune-stream-http/tests/lhc_sonde_puis_lache_4661.rs`.
use super::*;
use crate::db::zone_repo::ZoneRepo;
use crate::playback::{NowPlaying, PlayState};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Les chiffres du journal du 24/09/2026 (fil 1912).
const DUREE_PISTE_MS: u64 = 297_680;
const TAILLE_FICHIER: u64 = 52_510_796;
/// 851 968 (requête sans `Range`) + 720 896 (`bytes=44-`).
const OCTETS_SERVIS: u64 = 1_572_864;
const WALL_A_LA_COUPURE_S: u64 = 33;
const ARRET_A_LA_COUPURE_S: u64 = 31;

// ───────────────────────── 1. la décision pure ─────────────────────────

#[test]
fn la_mesure_du_fil_1912_est_un_demarrage_mort() {
    assert!(
        !decisions::demarrage_mort("dlna", OCTETS_SERVIS),
        "prémisse : la règle de #2394 seule ne voit pas ce cas (1,5 Mo servis)"
    );
    assert!(
        !decisions::reprise_apres_renderer_cale_autorisee(
            None,
            0,
            DUREE_PISTE_MS,
            OCTETS_SERVIS,
            Some(TAILLE_FICHIER)
        ),
        "prémisse : la reprise de #4645 refuse une piste qui n'a jamais joué"
    );
    assert!(
        decisions::demarrage_mort_apres_sondage("dlna", 0, OCTETS_SERVIS, Some(TAILLE_FICHIER)),
        "position jamais sortie de 0, 3 % du fichier tiré : c'est un démarrage \
         mort, il doit être relancé comme tel (fil 1912)"
    );
}

#[test]
fn le_demarrage_mort_apres_sondage_ne_rejoue_rien_qui_ait_pu_jouer() {
    // Le renderer a annoncé une position : c'est un décrochage EN COURS de
    // lecture, qui relève de la reprise à la position atteinte (#4645).
    assert!(!decisions::demarrage_mort_apres_sondage(
        "dlna",
        213_000,
        OCTETS_SERVIS,
        Some(TAILLE_FICHIER)
    ));
    // Fichier servi EN ENTIER à un renderer muet sur sa position : il a pu le
    // jouer jusqu'au bout (fil 1877). Le rejouer serait une faute.
    assert!(!decisions::demarrage_mort_apres_sondage(
        "dlna",
        0,
        TAILLE_FICHIER,
        Some(TAILLE_FICHIER)
    ));
    assert!(!decisions::demarrage_mort_apres_sondage(
        "dlna",
        0,
        TAILLE_FICHIER + 720_896,
        Some(TAILLE_FICHIER)
    ));
    // Taille inconnue : on ne relance pas sur une ignorance (#2394).
    assert!(!decisions::demarrage_mort_apres_sondage(
        "dlna",
        0,
        OCTETS_SERVIS,
        None
    ));
    // Zéro octet : c'est la règle de #2394, pas celle-ci.
    assert!(!decisions::demarrage_mort_apres_sondage(
        "dlna",
        0,
        0,
        Some(TAILLE_FICHIER)
    ));
    // Seul le DLNA porte la relance Pause→Stop→Play.
    assert!(!decisions::demarrage_mort_apres_sondage(
        "chromecast",
        0,
        OCTETS_SERVIS,
        Some(TAILLE_FICHIER)
    ));
}

// ───────────────────── 2. le BRANCHEMENT dans tick() ──────────────────────

/// Le darTZeel LHC-208 vu par le sondeur après l'abandon : `Stopped`,
/// position 0, durée connue.
struct Lhc {
    status: Arc<std::sync::Mutex<OutputStatus>>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl OutputTarget for Lhc {
    fn name(&self) -> &str {
        "LHC-55"
    }
    fn device_id(&self) -> &str {
        "dlna:lhc-55"
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
    _scratch: crate::test_scratch::ScratchDir,
}

impl Banc {
    /// La piste 11 de Sevy, adoptée par l'enchaînement gapless, que le LHC a
    /// sondée puis lâchée. `octets_servis` est le compte que la session du
    /// flux rend au sondeur.
    async fn scene(octets_servis: u64, wall_secs: u64) -> Self {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone = ZoneRepo::with_backend(db.clone())
            .create("DarTZeel LHC 208", Some("dlna"), Some("dlna:lhc-55"))
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
        let scratch = crate::test_scratch::scratch_dir("tune-sonde-4661");
        let fichier = scratch.join("close-that-gap.wav");
        std::fs::write(&fichier, b"RIFF").unwrap();
        let sid = streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 44_100,
                    bit_depth: 16,
                    channels: 2,
                    file_size: Some(TAILLE_FICHIER),
                    duration_ms: Some(DUREE_PISTE_MS),
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
                    title: "Close That Gap".into(),
                    artist_name: Some("Mecca".into()),
                    source: "local".into(),
                    stream_id: Some(sid),
                    duration_ms: DUREE_PISTE_MS as i64,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone, 11, 4980).await;
        let mut ps = ZonePollState::new(playback.get_state(zone).await.track_generation);
        ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(wall_secs));
        // Chargée avec la piste PRÉCÉDENTE : l'enchaînement gapless n'ouvre
        // pas de nouvelle grâce de chargement (la coupure du journal tombe
        // 33 s après l'adoption, sous les 45 s de cette grâce).
        ps.track_loaded_at = Instant::now() - Duration::from_secs(wall_secs + 300);
        Self {
            poller,
            stops,
            zone,
            polls: HashMap::from([(zone, ps)]),
            idle: HashMap::new(),
            _scratch: scratch,
        }
    }

    async fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.poller
                .tick(&mut self.polls, &mut self.idle, &Instant::now())
                .await;
        }
    }

    /// Amener la zone au seuil d'échec, reculer l'horloge d'arrêt de ce que
    /// le terrain a mesuré, et laisser le bras décider. Mêmes deux pièges que
    /// le banc de `fin_de_piste_a_l_horloge_4661` : seuil en TOURS et
    /// plancher en SECONDES cumulés, et un premier tour dans le bras qui
    /// amorce `last_bytes_sent` sans pouvoir conclure `ASec`.
    async fn arret_de(&mut self, arret_secs: u64) {
        self.ticks(STOPPED_FAILURE_THRESHOLD as usize + 1).await;
        let ps = self.polls.get_mut(&self.zone).expect("la zone du banc");
        assert!(
            ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD,
            "le banc n'a pas atteint le seuil en TOURS : rien ne serait jugé"
        );
        ps.premier_arret_a = Instant::now().checked_sub(Duration::from_secs(arret_secs));
        self.ticks(1).await;
        assert!(
            self.polls
                .get(&self.zone)
                .is_some_and(|p| p.last_bytes_sent != 0),
            "le bras du seuil d'échec n'a pas été atteint : l'épreuve ne \
             garderait rien"
        );
        self.ticks(1).await;
        assert!(
            !self.polls.contains_key(&self.zone),
            "le bras n'a rien tranché : la coupure (ou la relance) retire \
             l'état de sondage de la zone"
        );
    }

    async fn relance_armee(&self) -> bool {
        self.poller
            .relances_demarrage_mort
            .lock()
            .await
            .contains_key(&self.zone)
    }
}

/// LA SCÈNE DU FIL 1912 — le LHC a sondé la piste puis l'a lâchée, sa
/// position n'a jamais quitté 0. La zone ne doit plus être simplement
/// coupée : la relance du démarrage mort doit s'armer.
#[tokio::test]
async fn un_lhc_qui_sonde_puis_lache_la_piste_est_relance() {
    let mut b = Banc::scene(OCTETS_SERVIS, WALL_A_LA_COUPURE_S).await;
    b.arret_de(ARRET_A_LA_COUPURE_S).await;
    assert!(
        b.relance_armee().await,
        "🔴 fil 1912 : 1 572 864 octets servis sur 52 510 796, position jamais \
         sortie de 0 — le sondeur coupait la zone sans rien tenter. La relance \
         Pause→Stop→Play du démarrage mort doit s'armer"
    );
}

/// CONTRE-GARDE — le fichier est chez le renderer EN ENTIER et l'horloge de
/// la piste est passée : un renderer muet sur sa position a pu le jouer
/// jusqu'au bout (fil 1877). On coupe comme avant, on ne rejoue pas.
#[tokio::test]
async fn un_fichier_servi_en_entier_n_est_pas_rejoue() {
    let mut b = Banc::scene(TAILLE_FICHIER + 720_896, 400).await;
    b.arret_de(120).await;
    assert!(
        !b.relance_armee().await,
        "fichier servi en entier, piste finie à l'horloge : aucune relance"
    );
    assert_eq!(
        b.poller.playback.get_state(b.zone).await.state,
        PlayState::Stopped
    );
    assert!(b.stops.load(Ordering::SeqCst) > 0, "la zone est coupée");
}

/// CONTRE-GARDE — la fenêtre de la relance tient : un second échec dans les
/// trois minutes coupe la zone, on ne martèle pas un renderer qui refuse.
#[tokio::test]
async fn un_second_echec_dans_la_fenetre_coupe_la_zone() {
    let mut b = Banc::scene(OCTETS_SERVIS, WALL_A_LA_COUPURE_S).await;
    let recente = Instant::now() - Duration::from_secs(10);
    b.poller
        .relances_demarrage_mort
        .lock()
        .await
        .insert(b.zone, recente);
    b.arret_de(ARRET_A_LA_COUPURE_S).await;
    assert_eq!(
        b.poller.relances_demarrage_mort.lock().await.get(&b.zone),
        Some(&recente),
        "relance refusée dans la fenêtre : l'horodatage ne doit pas bouger"
    );
    assert_eq!(
        b.poller.playback.get_state(b.zone).await.state,
        PlayState::Stopped
    );
}
