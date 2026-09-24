//! #4661 — un fichier servi EN ENTIER se finit à l'HORLOGE DE PISTE, pas au
//! bout d'un forfait de deux minutes.
//!
//! ## Ce que #4480 a déjà réglé, et ce qu'il laisse
//!
//! La v0.9.163 a livré #4480 : la garde d'échec ne demande plus « le compteur
//! d'octets monte-t-il ? » mais « l'audio DÉJÀ LIVRÉE devance-t-elle la
//! position que le renderer annonce ? ». La patience accordée vaut
//! `min(avance_audio, AVANCE_AUDIO_BORNE_HAUTE_SECS)` — au plus **deux
//! minutes**, quelle que soit la piste.
//!
//! Ce forfait est PLAT. La scène du ticket (Sevy Tabroc, darTZeel LHC-208,
//! zone 10) :
//!
//! | instant | fait |
//! |---|---|
//! | piste + 0 s | WAV de 49 596 668 octets pour 281 160 ms de musique |
//! | piste + 89 s | fichier **entièrement servi** (`complet=true`) |
//! | piste + ~102 s | le renderer commence à annoncer `Stopped`, `peak_pos=0` |
//! | piste + ~222 s | #4480 coupe la zone : `min(avance, 120 s)` est écoulé |
//! | piste + 281 s | fin réelle de la musique |
//!
//! **59 secondes de musique encore dans le tampon du LHC**, et la file
//! s'arrête là. Or ces 59 s ne sont pas une inconnue : quand la durée est
//! connue ET que le fichier est chez le renderer en entier, l'horloge dit à
//! la seconde près quand la musique s'arrête.
//!
//! ## La règle posée ici
//!
//! Dans le seul bras qui coupe, et seulement sur un compteur MESURÉ et à sec,
//! la zone n'est pas coupée tant que les trois conditions tiennent ensemble :
//!
//! 1. le flux a été **servi en entier** — `octets_servis >= taille_du_flux`,
//!    [`fsm::flux_servi_en_entier`] ;
//! 2. l'horloge murale n'a pas dépassé `durée + END_MARGIN_MS` —
//!    [`decisions::tampon_du_renderer_peut_encore_jouer`] ;
//! 3. l'arrêt dure moins que [`HORLOGE_DE_PISTE_BORNE_HAUTE_SECS`].
//!
//! ⚠️ « Servi en entier » se lit **aux octets contre la taille du flux**,
//! jamais de la branche où l'on se trouve : la même branche `a_sec` s'arme
//! aussi sur un flux tronqué — mesuré à **70,5 %** servis sur un journal
//! réel. Conclure de la branche accorderait l'horloge à un renderer qui n'a
//! pas la musique. C'est la deuxième épreuve de ce fichier.
//!
//! La patience s'AJOUTE à celle de #4480, elle n'en retire aucune : toute
//! coupure que l'horloge n'épargne pas a lieu exactement comme avant.
use super::*;
use crate::db::zone_repo::ZoneRepo;
use crate::playback::{NowPlaying, PlayState};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Les chiffres du journal du 21/09/2026.
const DUREE_PISTE_MS: u64 = 281_160;
const TAILLE_FICHIER: u64 = 49_596_668;
/// Plus GRAND que le fichier : `bytes_sent` est monotone toutes connexions
/// confondues, une reprise `Range` le fait dépasser la taille. C'est pourquoi
/// le critère est `>=` et non `==`.
const OCTETS_SERVIS: u64 = 50_317_520;
/// Le flux tronqué du journal du 24/09 : 70,5 % servis, même branche `a_sec`.
const OCTETS_SERVIS_TRONQUES: u64 = 34_965_650;
/// L'instant où #4480 coupe : arrêt commencé vers 102 s de piste, patience
/// `min(avance, 120 s)` écoulée.
const WALL_A_LA_COUPURE_4480_S: u64 = 222;
const ARRET_A_LA_COUPURE_4480_S: u64 = 120;

// ───────────────────────── 1. les décisions pures ─────────────────────────

#[test]
fn servi_en_entier_se_lit_aux_octets_contre_la_taille_du_flux() {
    assert!(
        fsm::flux_servi_en_entier(Some(OCTETS_SERVIS), Some(TAILLE_FICHIER)),
        "50 317 520 octets servis pour un fichier de 49 596 668 : il est chez \
         le renderer en entier"
    );
    assert!(
        fsm::flux_servi_en_entier(Some(TAILLE_FICHIER), Some(TAILLE_FICHIER)),
        "l'égalité est le cas nominal"
    );
    assert!(
        !fsm::flux_servi_en_entier(Some(OCTETS_SERVIS_TRONQUES), Some(TAILLE_FICHIER)),
        "70,5 % servis : la même branche `a_sec`, mais le renderer n'a PAS la \
         musique"
    );
    // Les deux ignorances, et le zéro.
    assert!(
        !fsm::flux_servi_en_entier(None, Some(TAILLE_FICHIER)),
        "compteur inconnu : on ne conclut rien"
    );
    assert!(
        !fsm::flux_servi_en_entier(Some(OCTETS_SERVIS), None),
        "taille inconnue (radio, flux à la volée) : on ne conclut rien"
    );
    assert!(
        !fsm::flux_servi_en_entier(Some(0), Some(0)),
        "une taille nulle ne rend pas « entier » n'importe quel flux — un \
         démarrage mort reste un démarrage mort"
    );
}

#[test]
fn l_horloge_de_piste_dit_quand_la_musique_s_arrete() {
    assert!(
        decisions::tampon_du_renderer_peut_encore_jouer(WALL_A_LA_COUPURE_4480_S, DUREE_PISTE_MS),
        "à 222 s de piste — l'instant où #4480 coupe — il reste 59 s de musique"
    );
    assert!(decisions::tampon_du_renderer_peut_encore_jouer(
        283,
        DUREE_PISTE_MS
    ));
    assert!(
        !decisions::tampon_du_renderer_peut_encore_jouer(285, DUREE_PISTE_MS),
        "passé durée + END_MARGIN_MS, la musique est finie"
    );
    assert!(
        !decisions::tampon_du_renderer_peut_encore_jouer(10, 0),
        "durée inconnue : l'horloge ne tranche rien"
    );
}

#[test]
fn l_horloge_ne_couvre_l_arret_que_sur_un_flux_entier_et_borne() {
    let arret = |s| Some(Duration::from_secs(s));
    assert!(
        fsm::horloge_de_piste_couvre_l_arret(
            true,
            true,
            arret(ARRET_A_LA_COUPURE_4480_S),
            HORLOGE_DE_PISTE_BORNE_HAUTE_SECS
        ),
        "fichier entier, musique devant, arrêt sous le plafond : on attend"
    );
    assert!(
        !fsm::horloge_de_piste_couvre_l_arret(
            false,
            true,
            arret(ARRET_A_LA_COUPURE_4480_S),
            HORLOGE_DE_PISTE_BORNE_HAUTE_SECS
        ),
        "flux tronqué : l'horloge n'a pas voix au chapitre"
    );
    assert!(
        !fsm::horloge_de_piste_couvre_l_arret(
            true,
            false,
            arret(ARRET_A_LA_COUPURE_4480_S),
            HORLOGE_DE_PISTE_BORNE_HAUTE_SECS
        ),
        "plus de musique devant : l'horloge laisse couper"
    );
    assert!(
        !fsm::horloge_de_piste_couvre_l_arret(
            true,
            true,
            arret(HORLOGE_DE_PISTE_BORNE_HAUTE_SECS),
            HORLOGE_DE_PISTE_BORNE_HAUTE_SECS
        ),
        "au plafond, la patience s'arrête : une piste d'une heure ne garde pas \
         une zone morte ouverte une heure"
    );
    assert!(
        !fsm::horloge_de_piste_couvre_l_arret(true, true, None, HORLOGE_DE_PISTE_BORNE_HAUTE_SECS),
        "arrêt non mesuré : on n'accorde pas une patience qu'on ne sait pas borner"
    );
}

fn entree(consommation: fsm::ConsommationFlux, horloge: bool, avance: bool) -> fsm::StoppedInput {
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
        wall_elapsed: WALL_A_LA_COUPURE_4480_S,
        track_duration_ms: DUREE_PISTE_MS,
        stopped_ticks: STOPPED_FAILURE_THRESHOLD,
        natural_end: false,
        gapless_sent: false,
        realtime: true,
        can_internal_gapless: true,
        consommation,
        avance_audio_couvre_l_arret: avance,
        horloge_de_piste_couvre_l_arret: horloge,
        dlna_dsd_reached_end: false,
    }
}

#[test]
fn le_modele_attend_sur_l_horloge_et_coupe_sans_elle() {
    use fsm::{ConsommationFlux::*, StoppedOutcome};
    let attente = fsm::classify_stopped(&entree(ASec, true, false));
    assert_eq!(attente, StoppedOutcome::FailureWaitingHorloge);
    assert!(!attente.is_force_stop() && !attente.is_track_end());

    assert_eq!(
        fsm::classify_stopped(&entree(ASec, false, false)),
        StoppedOutcome::FailureStop,
        "sans l'horloge et sans l'avance, la coupure d'avant, intacte"
    );
    assert_eq!(
        fsm::classify_stopped(&entree(ASec, false, true)),
        StoppedOutcome::FailureWaitingAvance,
        "#4480 reste le filet quand l'horloge ne peut rien dire"
    );
    // L'horloge est le critère EXACT, l'avance le forfait : quand les deux
    // épargnent, c'est l'horloge qui porte l'issue — et donc le journal.
    assert_eq!(
        fsm::classify_stopped(&entree(ASec, true, true)),
        StoppedOutcome::FailureWaitingHorloge
    );
    // Les autres consommations ne la consultent pas.
    assert_eq!(
        fsm::classify_stopped(&entree(Consomme, false, false)),
        StoppedOutcome::FailureWaitingConsuming
    );
    assert_eq!(
        fsm::classify_stopped(&entree(Inconnue, false, false)),
        StoppedOutcome::FailureWaitingUnknown
    );
}

// ──────────────── 2. « écrit mais pas branché » : le bras réel ────────────

/// Le bras du seuil d'échec, lu à la compilation. Les épreuves ci-dessus
/// interrogent des fonctions PURES : elles ne verraient pas une règle
/// calculée puis jamais consultée dans `tick()`.
fn branche_du_seuil_d_echec() -> &'static str {
    const SOURCE: &str = include_str!("tick.rs");
    let debut = SOURCE
        .find("} else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD")
        .expect("le bras du seuil d'échec a disparu de tick.rs");
    let fin = SOURCE[debut..]
        .find("\"stopped_early_waiting\"")
        .expect("la branche suivante (stopped_early_waiting) a disparu de tick.rs");
    &SOURCE[debut..debut + fin]
}

#[test]
fn le_bras_de_production_consulte_l_horloge_de_piste_4661() {
    let branche = branche_du_seuil_d_echec();

    assert!(
        branche.contains("self.orchestrator.streamer_total_bytes(sid).await"),
        "sans la taille du flux, « servi en entier » ne peut PAS être établi \
         aux octets — et le déduire de la branche est justement le défaut \
         (70,5 % servis sur le même `a_sec`)"
    );
    assert!(
        branche.contains("fsm::flux_servi_en_entier(octets_servis, octets_total)"),
        "le bras doit comparer les octets SERVIS à la TAILLE du flux"
    );
    assert!(
        branche.contains("fsm::horloge_de_piste_couvre_l_arret("),
        "la règle est écrite mais le bras ne l'appelle pas (#4661)"
    );
    assert!(
        branche.contains("decisions::tampon_du_renderer_peut_encore_jouer("),
        "l'horloge doit se juger contre la durée de la piste, pas contre un \
         compteur de tours"
    );
    assert!(
        branche.contains("HORLOGE_DE_PISTE_BORNE_HAUTE_SECS"),
        "le plafond doit être la constante documentée, pas un littéral posé \
         sur place — c'est une décision de sûreté"
    );
    assert!(
        branche.contains("fsm_in.horloge_de_piste_couvre_l_arret = horloge_couvre_l_arret;"),
        "le verdict doit atteindre l'entrée de `classify_stopped`, sinon \
         l'arbre en ombre diverge du bras à chaque épargne"
    );
    assert!(
        branche.contains("fsm_in.avance_audio_couvre_l_arret = !famine_etablie;"),
        "#4661 s'AJOUTE à #4480, il ne le remplace pas : la borne des 120 s \
         reste le filet de tous les autres cas"
    );

    // 🔴 Calculer ne suffit pas : il faut BRANCHER, et AVANT la coupure —
    // après elle, l'épargne ne garderait plus rien. Chaque repère est
    // d'abord CHERCHÉ (un `find` absent rend `None`, et « A avant B » par
    // comparaison d'indices serait vrai gratuitement), puis situé.
    let epargne = branche
        .find("} else if horloge_couvre_l_arret {")
        .expect("le bras calcule l'horloge mais aucune branche d'épargne n'en sort (#4661)");
    let issue = branche
        .find("fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingHorloge);")
        .expect("l'épargne doit porter son issue, pas se contenter d'un journal");
    let journal = branche
        .find("\"flux_servi_en_entier_zone_non_coupee\"")
        .expect("une épargne muette se relit aussi mal que la coupure qu'elle remplace");
    let coupure = branche
        .find("fsm_actual = Some(fsm::StoppedOutcome::FailureStop);")
        .expect("la branche qui coupe a disparu du bras");
    assert!(
        epargne < coupure && issue < coupure && journal < coupure,
        "l'épargne de l'horloge doit passer AVANT la coupure"
    );

    // Et la coupure dit désormais contre quoi elle a jugé « servi en entier ».
    //
    // ⚠️ On découpe la SEULE branche qui coupe avant de chercher le champ :
    // `bytes_total` figure aussi dans le journal d'épargne ci-dessus, et un
    // `find` sur tout le bras y tomberait d'abord — la garde serait alors
    // satisfaite par la ligne qu'elle ne garde pas.
    let bras_qui_coupe = &branche[coupure..];
    let marqueur = bras_qui_coupe
        .find("\"playback_failure_stopping_zone\"")
        .expect("la ligne de coupure a disparu du bras");
    let champ = bras_qui_coupe
        .find("bytes_total = octets_total.unwrap_or(0)")
        .expect(
            "la ligne de coupure ne porte pas la taille du flux : `bytes_sent` \
             seul ne distingue pas 70 % servis de 101 %",
        );
    assert!(
        champ < marqueur,
        "`bytes_total` doit être un champ de la ligne de coupure, pas du texte \
         posé après elle"
    );
}

// ───────────────────── 3. le BRANCHEMENT dans tick() ──────────────────────

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
    _scratch: crate::test_scratch::ScratchDir,
}

impl Banc {
    /// La zone 10 de Sevy : le renderer annonce `Stopped` sans position, le
    /// fichier est déjà passé sur le fil, `wall_secs` secondes se sont
    /// écoulées depuis le début de la piste.
    async fn scene(wall_secs: u64, octets_servis: u64, duree_ms: u64) -> Self {
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
            duration_ms: duree_ms,
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
        let scratch = crate::test_scratch::scratch_dir("tune-horloge-4661");
        let fichier = scratch.join("mirror-mirror.wav");
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
                    duration_ms: duree_ms as i64,
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

    /// Amener la zone AU seuil d'échec, puis reculer l'horloge d'arrêt de ce
    /// que le terrain a mesuré, et laisser le bras décider.
    ///
    /// Deux pièges de banc, tous deux capables de rendre une épreuve verte
    /// pour la mauvaise raison — d'où les deux assertions :
    ///
    /// 1. les conditions de #4480 sont CUMULATIVES : un seuil en TOURS
    ///    (`STOPPED_FAILURE_THRESHOLD`) et un plancher en SECONDES
    ///    (`STOPPED_FAILURE_MIN_SECS`). Trente tours de banc prennent
    ///    quelques millisecondes ; sans ce recul, le bras du seuil ne
    ///    s'ouvrirait jamais et RIEN ne serait jugé ;
    /// 2. `ps.last_bytes_sent` n'est écrit QUE dans ce bras. Le premier tour
    ///    qui l'atteint compare donc les octets servis à `0` et conclut
    ///    `Consomme` — « le renderer tire » — quoi qu'il arrive. Il faut un
    ///    SECOND tour pour que la comparaison porte sur deux mesures et que
    ///    le verdict `ASec` — le seul qui coupe — apparaisse.
    async fn arret_de(&mut self, arret_secs: u64) {
        self.ticks(STOPPED_FAILURE_THRESHOLD as usize + 1).await;
        let ps = self.polls.get_mut(&self.zone).expect("la zone du banc");
        assert!(
            ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD,
            "le banc n'a pas atteint le seuil en TOURS : rien ne serait jugé"
        );
        ps.premier_arret_a = Instant::now().checked_sub(Duration::from_secs(arret_secs));
        // Premier tour dans le bras : amorce `last_bytes_sent`.
        self.ticks(1).await;
        assert_ne!(
            self.polls[&self.zone].last_bytes_sent, 0,
            "le bras du seuil d'échec n'a pas été atteint : le plancher en \
             SECONDES n'est pas franchi, l'épreuve ne garderait rien"
        );
        // Second tour : la consommation est enfin MESURÉE à sec, et le
        // verdict tombe.
        self.ticks(1).await;
    }

    async fn etat(&self) -> PlayState {
        self.poller.playback.get_state(self.zone).await.state
    }
}

/// POLARITÉ 1 — la scène du ticket, à l'instant exact où #4480 coupe : le
/// fichier est chez le renderer EN ENTIER et il reste 59 s de musique. La
/// zone ne doit plus être coupée.
#[tokio::test]
async fn un_fichier_servi_en_entier_garde_la_zone_jusqu_a_la_fin_de_la_piste() {
    let mut b = Banc::scene(WALL_A_LA_COUPURE_4480_S, OCTETS_SERVIS, DUREE_PISTE_MS).await;
    b.arret_de(ARRET_A_LA_COUPURE_4480_S).await;
    assert_eq!(
        b.etat().await,
        PlayState::Playing,
        "fichier servi en entier, 59 s de musique encore devant : la zone ne \
         doit pas être coupée"
    );
    assert_eq!(
        b.stops.load(Ordering::SeqCst),
        0,
        "aucun Stop envoyé au renderer"
    );
}

/// POLARITÉ 2 — LA CONTRE-GARDE. Même branche `a_sec`, même arrêt, même
/// piste : seul le flux est TRONQUÉ (70,5 % servis, le chiffre du journal du
/// 24/09). Le renderer n'a pas la musique — la zone doit être coupée, comme
/// avant. C'est l'épreuve qui interdit de déduire « servi en entier » de la
/// branche.
#[tokio::test]
async fn un_flux_tronque_coupe_toujours_la_zone() {
    let mut b = Banc::scene(
        WALL_A_LA_COUPURE_4480_S,
        OCTETS_SERVIS_TRONQUES,
        DUREE_PISTE_MS,
    )
    .await;
    b.arret_de(ARRET_A_LA_COUPURE_4480_S).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "70,5 % du fichier servis : le renderer n'a PAS de quoi jouer jusqu'au \
         bout, la zone doit toujours être coupée"
    );
}

/// POLARITÉ 2 bis — flux entier, mais la musique est FINIE à l'horloge. La
/// garde n'est pas désarmée, elle est seulement recalée : elle tire au bon
/// moment au lieu de tirer 59 s trop tôt.
#[tokio::test]
async fn passe_la_fin_de_la_piste_la_zone_est_coupee() {
    let mut b = Banc::scene(290, OCTETS_SERVIS, DUREE_PISTE_MS).await;
    b.arret_de(188).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "290 s sur une piste de 281 s : il n'y a plus rien à attendre"
    );
}

/// POLARITÉ 2 ter — une durée INCONNUE ne donne aucune prise à l'horloge :
/// verdict d'avant, coupure. (Ici l'avance d'audio n'épargne pas non plus,
/// la patience `min(avance, 120 s)` étant écoulée.)
#[tokio::test]
async fn sans_duree_connue_le_verdict_d_avant_tient() {
    let mut b = Banc::scene(WALL_A_LA_COUPURE_4480_S, OCTETS_SERVIS, 0).await;
    b.arret_de(ARRET_A_LA_COUPURE_4480_S).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "durée inconnue : l'horloge ne peut rien trancher, la borne plate reste"
    );
}

/// POLARITÉ 2 quater — LE PLAFOND. Une piste d'une heure servie en entier ne
/// garde pas une zone morte ouverte une heure : passé
/// [`HORLOGE_DE_PISTE_BORNE_HAUTE_SECS`], la coupure reprend ses droits.
#[tokio::test]
async fn le_plafond_ferme_une_zone_morte_sur_une_piste_tres_longue() {
    let mut b = Banc::scene(700, OCTETS_SERVIS, 3_600_000).await;
    b.arret_de(HORLOGE_DE_PISTE_BORNE_HAUTE_SECS + 1).await;
    assert_eq!(
        b.etat().await,
        PlayState::Stopped,
        "dix minutes d'arrêt : même avec 48 min de musique nominalement \
         devant, la zone ne reste pas ouverte indéfiniment"
    );
}
