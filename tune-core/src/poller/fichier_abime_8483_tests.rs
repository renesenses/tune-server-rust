//! #8483 — le banc : le VRAI décodeur note la coupure d'un fichier abîmé, le
//! VRAI sondeur (`tick`) voit la zone figée dessus et passe à la suivante.
//!
//! Le fichier abîmé est un WAV dont l'en-tête annonce 100 s et qui n'en
//! contient que 40 : la forme exacte du FLAC du fil 2769 (le conteneur
//! annonce plus de trames que le flux n'en porte), sans dépendre d'un
//! encodeur FLAC.
//!
//! Témoins :
//!  1. le décodeur en flux note la coupure à 40 s pour 100 s annoncées ;
//!  2. renderer `Playing` figé sur la coupure : suivante de la file, avis
//!     `track_file_damaged` (titre, `0:40`), non fatal, saut signalé ;
//!  3. sortie qui s'arrête sur la coupure (sortie locale) : même chose ;
//!  4. CONTRE-TÉMOIN : figé LOIN de la coupure, rien ne change ici (la garde
//!     #2116/#6702 reste seule juge) ;
//!  5. dernière piste de la file : fin de file, avec l'avis.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::{AutoplayMode, ZoneRepo};
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::outputs::traits::TransportState;
use crate::playback::PlaybackManager;
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "uuid:marantz-nr1200-8483";
const CADENCE: u32 = 8_000;
const ANNONCE_S: u32 = 100;
const REEL_S: u32 = 40;
const TITRE_ABIME: &str = "Lecture coupée";

/// Un WAV mono 16 bits dont le bloc `data` annonce `annonce_s` secondes et
/// n'en porte que `reel_s`.
fn ecrire_wav_tronque(chemin: &std::path::Path, annonce_s: u32, reel_s: u32) {
    use std::io::Write;
    let octets_annonces = CADENCE * 2 * annonce_s;
    let octets_reels = CADENCE * 2 * reel_s;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_annonces).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&CADENCE.to_le_bytes());
    v.extend_from_slice(&(CADENCE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_annonces.to_le_bytes());
    // Un signal non nul : du silence numérique pur ne prouve rien d'un
    // décodeur.
    for i in 0..(octets_reels / 2) {
        let ech = ((i % 64) as i16 - 32) * 256;
        v.extend_from_slice(&ech.to_le_bytes());
    }
    let mut f = std::fs::File::create(chemin).unwrap();
    f.write_all(&v).unwrap();
    f.flush().unwrap();
}

/// Ce que le décodeur en flux rend du fichier, jusqu'au bout — le chemin du
/// décodage-pour-niveaux du passthrough et de la sortie locale.
async fn decoder_jusqu_au_bout(chemin: &std::path::Path) {
    let chemin = chemin.to_str().unwrap().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let tache = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming(&chemin, None, None, tx, 16_384)
    });
    while rx.recv().await.is_some() {}
    tache
        .await
        .unwrap()
        .expect("le fichier abîmé se décode jusqu'à sa coupure");
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    chemin_abime: std::path::PathBuf,
    _dossier: tempfile::TempDir,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

/// Ce que l'écran a reçu pour la zone.
#[derive(Default, Debug)]
struct Recu {
    avis: Vec<serde_json::Value>,
    sauts: Vec<serde_json::Value>,
}

impl Banc {
    /// File `[abîmé, suivante]` (ou `[suivante, abîmé]` si `abime_en_dernier`),
    /// lecture lancée sur la piste abîmée.
    async fn monter(abime_en_dernier: bool) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zones = ZoneRepo::with_backend(db.clone());
        let zone_id = zones.create("Salon", Some("dlna"), Some(APPAREIL)).unwrap();
        zones
            .update_autoplay_mode(zone_id, AutoplayMode::Off)
            .unwrap();

        let dossier = tempfile::tempdir().unwrap();
        let chemin_abime = dossier.path().join("abime.wav");
        ecrire_wav_tronque(&chemin_abime, ANNONCE_S, REEL_S);
        let chemin_sain = dossier.path().join("suivante.wav");
        ecrire_wav_tronque(&chemin_sain, 30, 30);

        let depot = TrackRepo::with_backend(db.clone());
        let creer = |titre: &str, chemin: &std::path::Path, duree_s: u32| {
            let mut t = crate::db::models::Track::new(titre.to_string());
            t.file_path = Some(chemin.to_str().unwrap().to_string());
            t.format = Some("wav".into());
            t.sample_rate = Some(CADENCE as i32);
            t.bit_depth = Some(16);
            t.channels = 1;
            t.track_number = 1;
            t.duration_ms = duree_s as i64 * 1000;
            depot.create(&t).unwrap()
        };
        let abime = creer(TITRE_ABIME, &chemin_abime, ANNONCE_S);
        let suivante = creer("La suivante", &chemin_sain, 30);
        let file = if abime_en_dernier {
            vec![suivante, abime]
        } else {
            vec![abime, suivante]
        };
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &file)
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Marantz NR1200").with_type("dlna"),
        ));
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
            .play_from_queue(zone_id, if abime_en_dernier { 1 } else { 0 })
            .await
            .expect("la piste abîmée doit démarrer");
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db,
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);
        Self {
            poller,
            playback,
            outputs,
            zone_id,
            chemin_abime,
            _dossier: dossier,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
            recu,
        }
    }

    async fn sortie(&self, etat: TransportState, position_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(ANNONCE_S as u64 * 1000);
        mock.set_position(position_ms);
    }

    /// La piste joue depuis `REEL_S` secondes à l'horloge de Tune : sans
    /// cela, une position de 39 s vue dans la seconde du départ est écartée
    /// comme position périmée (`stale_start_position`) — à raison.
    async fn tic(&mut self) {
        for ps in self.poll_states.values_mut() {
            let depart = Instant::now() - Duration::from_secs(REEL_S as u64);
            ps.track_started_at = Some(depart);
            ps.track_loaded_at = depart;
        }
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Le gel dure depuis `secs` : l'horloge est un champ de l'état de
    /// sondage, on la date dans le passé (injection, pas un `sleep`).
    fn gel_depuis(&mut self, secs: u64) {
        if let Some(ps) = self.poll_states.get_mut(&self.zone_id)
            && let Some(g) = ps.gel_sur_coupure.as_mut()
        {
            g.depuis = Instant::now() - Duration::from_secs(secs);
        }
    }

    /// Renderer `Playing` figé à `position_ms` pendant cinq tours, dont le
    /// gel date de plus de `DUREE_DE_GEL`.
    async fn figer_en_lecture(&mut self, position_ms: u64) {
        self.sortie(TransportState::Playing, position_ms).await;
        for _ in 0..4 {
            self.tic().await;
        }
        self.gel_depuis(10);
        self.tic().await;
    }

    fn recu(&mut self) -> Recu {
        let mut recu = Recu::default();
        while let Ok(ev) = self.recu.try_recv() {
            if ev.data.get("zone_id").and_then(|v| v.as_i64()) != Some(self.zone_id) {
                continue;
            }
            match ev.event_type.as_str() {
                "zone.playback_error" => recu.avis.push(ev.data.clone()),
                "playback.track_skipped" => recu.sauts.push(ev.data.clone()),
                _ => {}
            }
        }
        recu
    }
}

fn verifier_l_avis(recu: &Recu) {
    assert_eq!(
        recu.avis.len(),
        1,
        "UN avis doit partir à l'écran : {recu:?}"
    );
    let avis = &recu.avis[0];
    assert_eq!(
        avis["code"],
        fichier_abime_8483::CODE_FICHIER_ABIME,
        "{avis}"
    );
    assert_eq!(avis["params"]["title"], TITRE_ABIME, "{avis}");
    assert_eq!(avis["params"]["position"], "0:40", "{avis}");
    assert_eq!(
        avis["fatal"], false,
        "la zone continue : avis NON fatal : {avis}"
    );
    // #7077 : aucune phrase française dans l'API, `error` n'est que le repli.
    assert!(
        avis["error"]
            .as_str()
            .unwrap()
            .contains("damaged from 0:40"),
        "{avis}"
    );
    assert_eq!(
        recu.sauts.len(),
        1,
        "le saut de piste doit être signalé : {recu:?}"
    );
    assert_eq!(
        recu.sauts[0]["reason"],
        fichier_abime_8483::CODE_FICHIER_ABIME
    );
}

#[tokio::test]
async fn t8483_le_decodeur_note_la_coupure_du_fichier_abime() {
    let banc = Banc::monter(false).await;
    decoder_jusqu_au_bout(&banc.chemin_abime).await;
    let c = crate::audio::coupure_connue_8483::coupure_de(banc.chemin_abime.to_str().unwrap())
        .expect("la coupure doit être notée par le décodeur en flux");
    assert_eq!(c.fin_ms, REEL_S as u64 * 1000);
    assert_eq!(c.annonce_ms, ANNONCE_S as u64 * 1000);
}

#[tokio::test]
async fn t8483_renderer_fige_sur_la_coupure_passe_a_la_suivante() {
    let mut banc = Banc::monter(false).await;
    decoder_jusqu_au_bout(&banc.chemin_abime).await;
    // Fil 2769 : le renderer se fige une seconde AVANT la coupure.
    banc.figer_en_lecture(39_000).await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Playing,
        "la zone ne s'arrête pas : {etat:?}"
    );
    assert_eq!(etat.queue_position, 1, "la SUIVANTE démarre : {etat:?}");
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.title.as_str()),
        Some("La suivante")
    );
    verifier_l_avis(&banc.recu());
}

#[tokio::test]
async fn t8483_sortie_arretee_sur_la_coupure_passe_a_la_suivante() {
    let mut banc = Banc::monter(false).await;
    decoder_jusqu_au_bout(&banc.chemin_abime).await;
    // La sortie locale joue jusqu'à la coupure, puis son flux s'épuise.
    banc.sortie(TransportState::Playing, 39_800).await;
    banc.tic().await;
    banc.tic().await;
    banc.sortie(TransportState::Stopped, 0).await;
    banc.tic().await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(etat.state, PlayState::Playing, "{etat:?}");
    assert_eq!(etat.queue_position, 1, "la SUIVANTE démarre : {etat:?}");
    verifier_l_avis(&banc.recu());
}

/// CONTRE-TÉMOIN : figé loin de la coupure, ce n'est pas la fin du fichier —
/// la piste reste, aucun avis (la garde #2116/#6702 reste seule juge).
#[tokio::test]
async fn t8483_fige_loin_de_la_coupure_ne_change_rien() {
    let mut banc = Banc::monter(false).await;
    decoder_jusqu_au_bout(&banc.chemin_abime).await;
    banc.figer_en_lecture(20_000).await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(etat.queue_position, 0, "{etat:?}");
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.title.as_str()),
        Some(TITRE_ABIME)
    );
    let recu = banc.recu();
    assert!(recu.avis.is_empty() && recu.sauts.is_empty(), "{recu:?}");
}

/// CONTRE-TÉMOIN : sans coupure connue (le décodeur n'a rien noté), un gel à
/// 39 s n'est pas une fin de piste.
#[tokio::test]
async fn t8483_sans_coupure_connue_rien_ne_change() {
    let mut banc = Banc::monter(false).await;
    // Pas de décodage : rien n'est su de ce fichier-là. Une autre coupure
    // connue garde le registre non vide, pour que la lecture en base ait lieu.
    crate::audio::coupure_connue_8483::noter_le_releve(
        "/nexiste/pas/autre-fichier-8483.flac",
        Some(1_000_000),
        10,
        44_100,
    );
    banc.figer_en_lecture(39_000).await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(etat.queue_position, 0, "{etat:?}");
    assert!(banc.recu().avis.is_empty());
}

#[tokio::test]
async fn t8483_fichier_abime_en_fin_de_file_termine_la_file_avec_l_avis() {
    let mut banc = Banc::monter(true).await;
    decoder_jusqu_au_bout(&banc.chemin_abime).await;
    banc.figer_en_lecture(39_000).await;

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Stopped,
        "dernière piste : fin de file : {etat:?}"
    );
    verifier_l_avis(&banc.recu());
}
