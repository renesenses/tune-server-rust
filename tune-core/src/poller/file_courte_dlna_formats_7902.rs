//! #7902 — une file de WAV de 4 s aux formats changeants s'arrêtait sur un
//! renderer DLNA, alors qu'elle allait au bout sur la sortie locale.
//!
//! Un renderer DLNA rend sa position à la seconde entière (`RelTime
//! 0:00:03`) : un fichier de 4,0 s n'est jamais vu au-delà de 3 000 ms, soit
//! 75 % de sa durée, sous les 80 % de `played_enough`. La grâce de chargement
//! du bras `Stopped` (`TRACK_LOAD_GRACE_SECS`, 45 s) retenait alors chaque
//! arrêt du renderer : la file restait muette 45 s après le titre. Le
//! changement de format n'y est pour rien ; le témoin le traverse quand même
//! (44,1/16 → 48/32 → 96/16 → …), comme la file du testeur.
//!
//! Aucun son : sortie factice `dlna`, fichiers silencieux dans un dossier
//! temporaire. Les témoins passent par la vraie porte, `tick()`.

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
use crate::outputs::traits::OutputTarget;
use crate::playback::{PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;

/// La durée des fichiers de la file du testeur.
const DUREE_MS: i64 = 4_000;

/// Ce qu'un renderer DLNA rapporte en fin de fichier de 4,0 s : 0:00:03.
const POSITION_A_LA_SECONDE_MS: u64 = 3_000;

/// Les formats de la file, dans l'ordre : le 3e passage est 48/32 → 96/16.
const FORMATS: [(u32, u16); 6] = [
    (44_100, 16),
    (44_100, 24),
    (48_000, 32),
    (96_000, 16),
    (96_000, 24),
    (192_000, 32),
];

const APPAREIL: &str = "dlna:temoin-7902";

/// Un WAV PCM silencieux de [`DUREE_MS`] au format demandé : le serveur lit
/// la durée du fichier lui-même, pas seulement celle de la base.
fn ecrire_wav(chemin: &std::path::Path, frequence: u32, bits: u16) {
    use std::io::Write;
    let canaux: u16 = 2;
    let bloc = canaux * bits / 8;
    let octets_data: u32 = frequence * (DUREE_MS as u32 / 1000) * u32::from(bloc);
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&canaux.to_le_bytes());
    v.extend_from_slice(&frequence.to_le_bytes());
    v.extend_from_slice(&(frequence * u32::from(bloc)).to_le_bytes());
    v.extend_from_slice(&bloc.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    let mut f = std::fs::File::create(chemin).unwrap();
    f.write_all(&v).unwrap();
    f.flush().unwrap();
}

struct Banc {
    poller: PositionPoller,
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    pistes: Vec<i64>,
    _fichiers: Vec<tempfile::NamedTempFile>,
}

impl Banc {
    /// Une zone DLNA dont la file porte six WAV de 4 s, un par format.
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();

        let mut fichiers = Vec::new();
        let mut pistes = Vec::new();
        for (frequence, bits) in FORMATS {
            let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
            ecrire_wav(fichier.path(), frequence, bits);
            let mut piste =
                crate::db::models::Track::new(format!("WAV {frequence} Hz {bits} bits"));
            piste.file_path = Some(fichier.path().to_string_lossy().into_owned());
            piste.format = Some("wav".into());
            piste.sample_rate = Some(frequence as i32);
            piste.bit_depth = Some(i32::from(bits));
            piste.channels = 2;
            piste.duration_ms = DUREE_MS;
            pistes.push(TrackRepo::with_backend(db.clone()).create(&piste).unwrap());
            fichiers.push(fichier);
        }
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes)
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Témoin 7902").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        playback.set_repeat(zone_id, RepeatMode::Off).await;
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
            zone_id,
            pistes,
            _fichiers: fichiers,
        }
    }

    async fn play_recus(&self) -> usize {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    async fn piste_en_cours(&self) -> Option<i64> {
        let etat = self.playback.get_state(self.zone_id).await;
        etat.now_playing.as_ref().and_then(|p| p.track_id)
    }

    /// Le titre en cours a fini sur le renderer : tout le fichier a été
    /// servi, le renderer est à l'arrêt et rapporte `pic` ms sur une durée
    /// de 4 s. Le chargement date de 4 s : la grâce de 45 s est ouverte.
    /// Le sondeur passe ensuite huit fois, sans aucune attente réelle.
    async fn fin_du_titre_vue_a(&self, pic: u64) {
        let etat = self.playback.get_state(self.zone_id).await;
        assert_eq!(etat.state, PlayState::Playing, "la zone joue");
        let sid = etat
            .now_playing
            .as_ref()
            .unwrap()
            .stream_id
            .clone()
            .unwrap();
        {
            let sessions = self.orchestrator.streamer.sessions_state();
            let sessions = sessions.lock().await;
            let session = sessions.get(&sid).unwrap();
            session
                .bytes_sent
                .store(session.info.file_size.unwrap(), Ordering::Relaxed);
        }
        {
            let outputs = self.outputs.lock().await;
            let arc = outputs.get(APPAREIL).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.stop().await.unwrap();
            mock.set_position(pic);
            mock.set_duration(DUREE_MS as u64);
        }
        let mut ps = ZonePollState::new(etat.track_generation);
        ps.track_started_at = Instant::now().checked_sub(Duration::from_secs(4));
        ps.track_loaded_at = Instant::now() - Duration::from_secs(4);
        ps.peak_position_ms = pic;
        ps.last_position_ms = pic;
        let mut polls = HashMap::from([(self.zone_id, ps)]);
        let mut idle = HashMap::new();
        for _ in 0..STOPPED_TICKS_THRESHOLD + 3 {
            self.poller
                .tick(&mut polls, &mut idle, &Instant::now())
                .await;
            let avance = self.piste_en_cours().await
                != Some(etat.now_playing.as_ref().unwrap().track_id.unwrap());
            if avance {
                break;
            }
        }
    }
}

/// **LE TÉMOIN DU TICKET** : la file de six WAV de 4 s aux formats changeants
/// va jusqu'au dernier titre sur un renderer DLNA qui rapporte 0:00:03 en
/// fin de fichier ; la sortie reçoit un Play par titre.
#[tokio::test]
async fn la_file_de_wav_courts_aux_formats_changeants_va_au_bout_en_dlna_7902() {
    let banc = Banc::monter().await;
    banc.orchestrator
        .play_from_queue(banc.zone_id, 0)
        .await
        .expect("le premier départ doit aboutir");
    assert_eq!(banc.play_recus().await, 1, "un seul play reçu au départ");

    for rang in 1..banc.pistes.len() {
        let (f_avant, b_avant) = FORMATS[rang - 1];
        let (f_apres, b_apres) = FORMATS[rang];
        banc.fin_du_titre_vue_a(POSITION_A_LA_SECONDE_MS).await;
        assert_eq!(
            banc.piste_en_cours().await,
            Some(banc.pistes[rang]),
            "#7902 : sur un renderer DLNA qui rapporte {POSITION_A_LA_SECONDE_MS} ms \
             à la fin d'un WAV de {DUREE_MS} ms, la file doit passer du titre {rang} \
             ({f_avant}/{b_avant}) au titre {} ({f_apres}/{b_apres}) sans attendre \
             la grâce de chargement de 45 s",
            rang + 1
        );
        assert_eq!(
            banc.play_recus().await,
            rang + 1,
            "la sortie reçoit vraiment le titre {} ({f_apres}/{b_apres})",
            rang + 1
        );
    }
}

/// Contre-témoin : un titre qui n'a pas démarré (aucune position rapportée)
/// garde sa grâce de chargement ; la file n'est pas sautée.
#[tokio::test]
async fn un_wav_court_qui_n_a_pas_demarre_garde_sa_grace_en_dlna_7902() {
    let banc = Banc::monter().await;
    banc.orchestrator
        .play_from_queue(banc.zone_id, 0)
        .await
        .expect("le premier départ doit aboutir");

    banc.fin_du_titre_vue_a(0).await;

    assert_eq!(
        banc.piste_en_cours().await,
        Some(banc.pistes[0]),
        "démarrage sans progression : conserver sa grâce, ne pas sauter la piste"
    );
    assert_eq!(banc.play_recus().await, 1);
}

/// La règle pure : un titre court a joué dès qu'il a passé la moitié de sa
/// durée ; un titre long, une durée inconnue ou un pic à zéro, non.
#[test]
fn titre_court_deja_avance_7902() {
    assert!(decisions::titre_court_deja_avance(4_000, 3_000));
    assert!(decisions::titre_court_deja_avance(4_000, 2_000));
    assert!(!decisions::titre_court_deja_avance(4_000, 1_999));
    assert!(!decisions::titre_court_deja_avance(4_000, 0));
    assert!(!decisions::titre_court_deja_avance(0, 3_000));
    assert!(!decisions::titre_court_deja_avance(240_000, 200_000));
}
