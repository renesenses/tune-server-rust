//! #6735 — Répéter la piste (Repeat One) relance un morceau de moins de 8 s.
//!
//! Fil 2358 (Micetecumseh, 1.0.0-rc6, zone locale) : le fichier d'essai RME
//! dure 3 836 ms. À sa fin, le sondeur relance la même position ; la garde
//! anti-double-appui de `play` (`RETAP_DEDUP_WINDOW`, 8 s, #1271) voit la
//! MÊME piste, sur une zone encore `Playing`, partie il y a moins de 8 s, et
//! coalesce : `orchestrator_play_retap_deduped_same_inflight_track`, rien
//! n'est rejoué. Sur un renderer réseau, la garde `DUPLICATE_NET_PLAY_WINDOW`
//! (12 s) avalerait la même relance plus loin.
//!
//! Les témoins passent par la vraie porte : `handle_track_end`, avec une
//! sortie factice qui compte les `play` qu'elle reçoit. Le geste d'un
//! contrôleur (`play_from_queue` répété) reste coalescé : #1271 n'est pas
//! rouvert.

use super::lire_ensuite_dans_la_fenetre_gapless::ecrire_wav;
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// La durée du fichier d'essai RME du journal.
const DUREE_MS: i64 = 3_836;

struct Banc {
    poller: PositionPoller,
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    appareil: String,
    zone_id: i64,
    _fichier: tempfile::NamedTempFile,
}

impl Banc {
    /// Une zone dont la file ne porte qu'UNE piste de 3,8 s, en Répéter la
    /// piste, et dont la sortie est du type demandé.
    async fn monter(type_de_sortie: &str, appareil: &str) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some(type_de_sortie), Some(appareil))
            .unwrap();

        let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        ecrire_wav(fichier.path());
        let mut piste =
            crate::db::models::Track::new("192 kHz 32 bit ADI-2 Pro Bit Test 4s".to_string());
        piste.file_path = Some(fichier.path().to_str().unwrap().to_string());
        piste.format = Some("wav".into());
        piste.sample_rate = Some(44_100);
        piste.bit_depth = Some(16);
        piste.channels = 2;
        piste.duration_ms = DUREE_MS;
        let track_id = TrackRepo::with_backend(db.clone()).create(&piste).unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &[track_id])
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(appareil, "Témoin 6735").with_type(type_de_sortie),
        ));
        let playback = Arc::new(PlaybackManager::new());
        playback.set_repeat(zone_id, RepeatMode::One).await;
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator.clone(),
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            orchestrator,
            playback,
            outputs,
            appareil: appareil.to_string(),
            zone_id,
            _fichier: fichier,
        }
    }

    /// Les `play` que la SORTIE a reçus : la question est « le morceau
    /// est-il reparti ? », et seul l'appareil peut y répondre.
    async fn play_recus(&self) -> usize {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    /// Le premier départ, comme un clic sur la piste.
    async fn lancer(&self) {
        self.orchestrator
            .play_from_queue(self.zone_id, 0)
            .await
            .expect("le premier départ doit aboutir");
        let etat = self.playback.get_state(self.zone_id).await;
        assert_eq!(etat.state, PlayState::Playing, "la zone joue");
        assert!(
            etat.last_play_started_at
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(8)),
            "le départ date de moins de 8 s : la fenêtre de la garde est ouverte"
        );
        assert_eq!(self.play_recus().await, 1, "un seul play reçu au départ");
    }

    /// La fin de piste constatée par le sondeur, par sa vraie porte.
    async fn fin_de_piste(&self) {
        let etat = self.playback.get_state(self.zone_id).await;
        self.poller.handle_track_end(self.zone_id, &etat).await;
    }
}

async fn repeter_la_piste_relance_le_morceau_court(type_de_sortie: &str, appareil: &str) {
    let banc = Banc::monter(type_de_sortie, appareil).await;
    banc.lancer().await;

    banc.fin_de_piste().await;

    assert_eq!(
        banc.play_recus().await,
        2,
        "sortie {type_de_sortie} : Répéter la piste doit relancer un morceau de \
         {DUREE_MS} ms à sa fin ; la relance du sondeur a été avalée par une \
         garde anti-double-appui (RETAP_DEDUP_WINDOW / DUPLICATE_NET_PLAY_WINDOW) \
         qui l'a prise pour un second appui (#6735)"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "la zone joue toujours après la relance"
    );
}

/// **LA MESURE DU TICKET**, sur la sortie du testeur : une zone locale. Seule
/// la garde d'entrée (`RETAP_DEDUP_WINDOW`) s'y applique.
#[tokio::test]
async fn repeter_la_piste_relance_un_morceau_de_moins_de_8_s_en_local() {
    repeter_la_piste_relance_le_morceau_court("local", "local:temoin-6735").await;
}

/// Le même geste sur un renderer réseau : la garde de `play_inner`
/// (`DUPLICATE_NET_PLAY_WINDOW`, 12 s) s'y ajoute et avalait elle aussi la
/// relance.
#[tokio::test]
async fn repeter_la_piste_relance_un_morceau_de_moins_de_8_s_en_dlna() {
    repeter_la_piste_relance_le_morceau_court("dlna", "dlna:temoin-6735").await;
}

/// Contre-témoin de #1271 : un DOUBLE APPUI d'un contrôleur sur la même
/// piste, quelques secondes après son départ, reste coalescé. L'exemption ne
/// vaut que pour la fin de piste du sondeur.
#[tokio::test]
async fn le_double_appui_d_un_controleur_reste_coalesce() {
    for (type_de_sortie, appareil) in [("local", "local:temoin-6735"), ("dlna", "dlna:temoin-6735")]
    {
        let banc = Banc::monter(type_de_sortie, appareil).await;
        banc.lancer().await;

        let second = banc
            .orchestrator
            .play_from_queue(banc.zone_id, 0)
            .await
            .expect("le second appui rend un résultat");

        assert!(
            !second.output_sent,
            "sortie {type_de_sortie} : le second appui ne doit rien renvoyer"
        );
        assert_eq!(
            banc.play_recus().await,
            1,
            "sortie {type_de_sortie} : un double appui ne relance pas la piste (#1271)"
        );
    }
}

/// #7097 — passer par tick(), pas seulement handle_track_end : la grâce
/// de chargement ne doit pas avaler la fin d'un fichier plus court que 5 s.
async fn la_fin_courte_est_examinee_avant_45_secondes_7097(pic: u64, doit_avancer: bool) {
    use crate::outputs::traits::OutputTarget;
    use std::sync::atomic::Ordering;
    let b = Banc::monter("openhome", "mock-openhome-7097").await;
    // Deux pistes distinctes, fichier et durée connus ; la sortie est factice.
    let db = b.orchestrator.db.clone();
    let premier = PlayQueueRepo::with_backend(db.clone())
        .get_queue(b.zone_id)
        .unwrap()[0]
        .track_id;
    let second_fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav(second_fichier.path());
    let mut seconde = crate::db::models::Track::new("Deuxième WAV court 7097".into());
    seconde.file_path = Some(second_fichier.path().to_string_lossy().into_owned());
    seconde.format = Some("wav".into());
    seconde.duration_ms = DUREE_MS;
    seconde.sample_rate = Some(44_100);
    seconde.bit_depth = Some(16);
    let suivant = TrackRepo::with_backend(db.clone())
        .create(&seconde)
        .unwrap();
    PlayQueueRepo::with_backend(db)
        .set_queue(b.zone_id, &[premier, suivant])
        .unwrap();
    b.playback.set_repeat(b.zone_id, RepeatMode::Off).await;
    b.lancer().await;
    let etat = b.playback.get_state(b.zone_id).await;
    let sid = etat
        .now_playing
        .as_ref()
        .unwrap()
        .stream_id
        .as_ref()
        .unwrap();
    {
        let sessions = b.orchestrator.streamer.sessions_state();
        let sessions = sessions.lock().await;
        let session = sessions.get(sid).unwrap();
        session
            .bytes_sent
            .store(session.info.file_size.unwrap(), Ordering::Relaxed);
    }
    {
        let outputs = b.outputs.lock().await;
        let arc = outputs.get(&b.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.stop().await.unwrap();
        mock.set_position(pic);
        // Comme le rapport : OpenHome ne donne pas sa durée, la file la porte.
        mock.set_duration(0);
    }
    let mut ps = ZonePollState::new(etat.track_generation);
    ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(4));
    ps.track_loaded_at = Instant::now() - Duration::from_secs(4);
    ps.peak_position_ms = pic;
    ps.last_position_ms = pic;
    let mut polls = HashMap::from([(b.zone_id, ps)]);
    let mut idle = HashMap::new();
    // Cinq arrêts de la sortie, seuil normal inchangé ; le banc reste bien
    // dans la fenêtre des 45 s et ne provoque aucune attente réelle.
    for _ in 0..STOPPED_TICKS_THRESHOLD {
        b.poller.tick(&mut polls, &mut idle, &Instant::now()).await;
    }
    let apres = b.playback.get_state(b.zone_id).await;
    if doit_avancer {
        assert_eq!(
            apres.now_playing.as_ref().and_then(|p| p.track_id),
            Some(suivant),
            "#7097 : la fin d'un WAV de moins de cinq secondes ne doit pas attendre toute la grâce de 45 s"
        );
        assert_eq!(
            b.play_recus().await,
            2,
            "la sortie reçoit vraiment le titre suivant"
        );
    } else {
        assert_eq!(
            apres.now_playing.as_ref().and_then(|p| p.track_id),
            Some(premier),
            "démarrage sans progression : conserver sa grâce, ne pas sauter la piste"
        );
        assert_eq!(b.play_recus().await, 1);
    }
}

#[tokio::test]
async fn le_sondeur_avance_apres_un_wav_court_sans_attendre_45_secondes_7097() {
    la_fin_courte_est_examinee_avant_45_secondes_7097(3_500, true).await;
}

#[tokio::test]
async fn le_sondeur_garde_la_grace_d_un_wav_qui_n_a_pas_demarre_7097() {
    la_fin_courte_est_examinee_avant_45_secondes_7097(0, false).await;
}
