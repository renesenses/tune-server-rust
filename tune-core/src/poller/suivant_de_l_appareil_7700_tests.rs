//! #7700 — « suivant » à la télécommande d'un renderer DLNA (Eversolo
//! DMP-A6) : la file de Tune doit enchaîner en quelques secondes.
//!
//! Le banc est celui de production : base SQLite en mémoire avec ses
//! migrations, orchestrateur, gestionnaire de flux, gestionnaire de lecture
//! et `tick()` du sondeur. Seul l'appareil est simulé : il rapporte l'état,
//! la position et l'URI qu'on lui donne, comme un A6 interrogé par
//! `GetTransportInfo` / `GetPositionInfo`, et compte les lectures reçues.
//! Un tour de `tick()` vaut un sondage, soit une seconde en production
//! (`POLL_INTERVAL_MS`).

use super::suivant_de_l_appareil_7700::{
    AVANCE_MINIMALE_MS, ArretRapporte, PIC_MINIMAL_MS, TOURS_D_ARRET_DU_SUIVANT,
    depart_de_l_appareil, le_renderer_a_lache_un_flux_qu_il_avait_en_main,
};
use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::playback::{NowPlaying, PlaybackManager};
use crate::poller::decisions::TenueDuRenderer;
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEVICE_ID: &str = "dlna:eversolo-dmp-a6-7700";
const DUREE_MS: u64 = 240_000;
/// 40 % de la piste : loin de la fin, `played_enough` est faux.
const PIC_MS: u64 = 96_000;
/// Débit nominal du flux du banc : 24 Mo pour 240 s, 100 000 octets/s.
const TAILLE_DU_FLUX: u64 = 24_000_000;
const OCTETS_PAR_SECONDE: u64 = TAILLE_DU_FLUX * 1_000 / DUREE_MS;
/// Sondages accordés pour enchaîner : « quelques secondes ».
const TOURS_ACCORDES: usize = 4;

/// L'A6 vu par DLNA : état, position et URI fixés par le banc.
struct EversoloA6 {
    rapporte: Arc<std::sync::Mutex<(TransportState, u64, Option<String>)>>,
    lectures: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl OutputTarget for EversoloA6 {
    fn name(&self) -> &str {
        "Eversolo DMP-A6"
    }
    fn device_id(&self) -> &str {
        DEVICE_ID
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        self.lectures.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        self.lectures.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        let (state, position_ms, current_uri) = self.rapporte.lock().unwrap().clone();
        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms: DUREE_MS,
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
    streamer: Arc<AudioStreamer>,
    zone_id: i64,
    flux: String,
    rapporte: Arc<std::sync::Mutex<(TransportState, u64, Option<String>)>>,
    lectures: Arc<AtomicU32>,
    poll_states: HashMap<i64, ZonePollState>,
    idle_backoff: HashMap<i64, IdlePollBackoff>,
    _fichiers: tempfile::TempDir,
}

impl Banc {
    /// Une zone DLNA qui joue la première de deux pistes de sa file.
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(DEVICE_ID))
            .unwrap();

        // Deux vraies pistes en base et sur disque : l'enchaînement va
        // jusqu'à l'appareil.
        let fichiers = tempfile::tempdir().unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Rob');
            INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Salon', 1, 2026);",
        )
        .unwrap();
        for (id, titre) in [(1_i64, "Potion"), (2, "La suivante")] {
            let fichier = fichiers.path().join(format!("{id}.wav"));
            let mut wav =
                crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(DUREE_MS))
                    .to_vec();
            wav.resize(wav.len() + 44100 * 4, 0);
            std::fs::write(&fichier, wav).unwrap();
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, duration_ms)
                VALUES (?, ?, 1, 1, ?, 'wav', 240000)",
                &[&id, &titre, &fichier.to_str().unwrap()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO queue_items (zone_id, position, track_id, source)
                VALUES (?, ?, ?, 'local')",
                &[&zone_id, &(id - 1), &id],
            )
            .unwrap();
        }

        // La session de la piste en cours, telle que l'A6 la tire.
        let streamer = Arc::new(AudioStreamer::new(0));
        let source = fichiers.path().join("1.flac");
        std::fs::write(&source, b"fLaC").unwrap();
        let flux = streamer
            .create_file_session(
                StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    file_size: Some(TAILLE_DU_FLUX),
                    duration_ms: Some(DUREE_MS),
                    ..Default::default()
                },
                source.to_string_lossy().into_owned(),
                false,
            )
            .await;

        let rapporte = Arc::new(std::sync::Mutex::new((
            TransportState::Playing,
            PIC_MS,
            Some(format!("http://192.168.1.10:8888/stream/{flux}")),
        )));
        let lectures = Arc::new(AtomicU32::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(EversoloA6 {
            rapporte: rapporte.clone(),
            lectures: lectures.clone(),
        }));

        let playback = Arc::new(PlaybackManager::new());
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
                    duration_ms: DUREE_MS as i64,
                    stream_id: Some(flux.clone()),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 2).await;
        ZoneRepo::with_backend(db.clone())
            .save_play_state(zone_id, "playing")
            .unwrap();

        Self {
            poller,
            playback,
            streamer,
            zone_id,
            flux,
            rapporte,
            lectures,
            poll_states: HashMap::new(),
            idle_backoff: HashMap::new(),
            _fichiers: fichiers,
        }
    }

    fn notre_uri(&self) -> String {
        format!("http://192.168.1.10:8888/stream/{}", self.flux)
    }

    fn appareil(&self, state: TransportState, position_ms: u64, uri: Option<&str>) {
        *self.rapporte.lock().unwrap() = (state, position_ms, uri.map(str::to_string));
    }

    /// L'audio que l'A6 a déjà reçue, en secondes de musique.
    async fn audio_servie_ms(&self, ms: u64) {
        let sessions = self.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(&self.flux)
            .unwrap()
            .bytes_sent
            .store(ms * OCTETS_PAR_SECONDE / 1_000, Ordering::Relaxed);
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

    /// La piste joue depuis `PIC_MS` : le sondeur l'a vue jouer jusque-là.
    /// L'horloge de piste est reculée d'autant, comme en production.
    async fn joue_depuis_96_s(&mut self) {
        let uri = self.notre_uri();
        self.appareil(TransportState::Playing, PIC_MS, Some(&uri));
        self.tours(1).await;
        let ps = self
            .poll_states
            .get_mut(&self.zone_id)
            .expect("le sondeur suit la zone");
        let depuis = Instant::now() - Duration::from_millis(PIC_MS);
        ps.track_started_at = Some(depuis);
        ps.track_loaded_at = depuis;
        ps.peak_position_ms = PIC_MS;
        ps.last_position_ms = PIC_MS;
        self.tours(1).await;
    }

    async fn titre_en_cours(&self) -> String {
        self.playback
            .get_state(self.zone_id)
            .await
            .now_playing
            .map(|np| np.title)
            .unwrap_or_default()
    }

    /// Sondages, au plus `TOURS_ACCORDES`, avant que la suivante parte.
    async fn tours_jusqu_a_la_suivante(&mut self) -> Option<usize> {
        for n in 1..=TOURS_ACCORDES {
            self.tours(1).await;
            if self.lectures.load(Ordering::SeqCst) > 0 {
                return Some(n);
            }
        }
        None
    }
}

/// LE CONSTAT DE #7700 : en milieu de piste, l'A6 lâche notre flux
/// (`Stopped`, position retombée à 0) avec de l'audio encore en main. La
/// piste suivante de la file doit partir vers l'appareil en quelques
/// sondages, et la zone l'afficher.
#[tokio::test]
async fn un_suivant_a_la_telecommande_enchaine_la_file_en_quelques_secondes_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(PIC_MS + 30_000).await;
    banc.joue_depuis_96_s().await;
    assert_eq!(banc.lectures.load(Ordering::SeqCst), 0);

    let uri = banc.notre_uri();
    banc.appareil(TransportState::Stopped, 0, Some(&uri));
    let tours = banc.tours_jusqu_a_la_suivante().await;

    assert!(
        tours.is_some(),
        "#7700 : « suivant » pressé sur l'appareil à 96 s d'une piste de 240 s — \
         la suivante n'est toujours pas demandée après {TOURS_ACCORDES} sondages \
         (le sondeur attend 30 s d'arrêt, puis relance la même piste)"
    );
    assert_eq!(
        banc.titre_en_cours().await,
        "La suivante",
        "#7700 : c'est la piste SUIVANTE de la file qui doit jouer, pas la même relancée"
    );
}

/// Même geste, l'A6 vide son URI (`NO_MEDIA_PRESENT`, lu `Stopped`) sans
/// remettre sa position à zéro : il a quitté notre piste.
#[tokio::test]
async fn un_suivant_qui_vide_l_uri_enchaine_aussi_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(PIC_MS + 30_000).await;
    banc.joue_depuis_96_s().await;

    banc.appareil(TransportState::Stopped, PIC_MS, Some(""));
    let tours = banc.tours_jusqu_a_la_suivante().await;

    assert!(
        tours.is_some(),
        "#7700 : l'appareil a vidé son URI en milieu de piste, la suivante doit partir"
    );
    assert_eq!(banc.titre_en_cours().await, "La suivante");
}

/// GARDE #4645 : un renderer qui s'arrête parce qu'il est À SEC (l'audio
/// servie ne dépasse pas sa position) n'a pas fait « suivant ». La file
/// n'avance pas : sa reprise à la position atteinte reste son chemin.
#[tokio::test]
async fn un_renderer_a_sec_ne_fait_pas_avancer_la_file_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(PIC_MS + 1_000).await;
    banc.joue_depuis_96_s().await;

    let uri = banc.notre_uri();
    banc.appareil(TransportState::Stopped, 0, Some(&uri));
    banc.tours(TOURS_ACCORDES + 2).await;

    assert_eq!(
        banc.lectures.load(Ordering::SeqCst),
        0,
        "un renderer à court d'audio ne doit pas être pris pour un « suivant »"
    );
    assert_eq!(banc.titre_en_cours().await, "Potion");
}

/// GARDE #4661 : le fichier est chez l'appareil EN ENTIER et il annonce
/// `Stopped` à 0 avec notre URI. Un DarTZeel LHC-208 fait exactement cela
/// pendant qu'il joue encore son tampon : la file n'avance pas.
#[tokio::test]
async fn un_fichier_servi_en_entier_ne_fait_pas_avancer_la_file_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(DUREE_MS).await;
    banc.joue_depuis_96_s().await;

    let uri = banc.notre_uri();
    banc.appareil(TransportState::Stopped, 0, Some(&uri));
    banc.tours(TOURS_ACCORDES + 2).await;

    assert_eq!(banc.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(banc.titre_en_cours().await, "Potion");
}

/// GARDE : un seul sondage `Stopped`, puis l'appareil rejoue : rien ne part.
#[tokio::test]
async fn un_arret_d_un_seul_sondage_ne_fait_rien_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(PIC_MS + 30_000).await;
    banc.joue_depuis_96_s().await;

    let uri = banc.notre_uri();
    banc.appareil(TransportState::Stopped, 0, Some(&uri));
    banc.tours(1).await;
    banc.appareil(TransportState::Playing, PIC_MS + 1_000, Some(&uri));
    banc.tours(3).await;

    assert_eq!(banc.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(banc.titre_en_cours().await, "Potion");
}

/// GARDE : une autre application prend l'appareil (son URI n'est pas la
/// nôtre) : ce n'est pas un « suivant », la file n'avance pas.
#[tokio::test]
async fn une_autre_application_sur_l_appareil_ne_fait_pas_avancer_la_file_7700() {
    let mut banc = Banc::monter().await;
    banc.audio_servie_ms(PIC_MS + 30_000).await;
    banc.joue_depuis_96_s().await;

    banc.appareil(
        TransportState::Stopped,
        0,
        Some("http://192.168.1.20:8096/Audio/abc/stream.flac"),
    );
    banc.tours(TOURS_ACCORDES + 2).await;

    assert_eq!(banc.lectures.load(Ordering::SeqCst), 0);
    assert_eq!(banc.titre_en_cours().await, "Potion");
}

fn arret<'a>(tenue: &'a TenueDuRenderer) -> ArretRapporte<'a> {
    ArretRapporte {
        output_type: "dlna",
        realtime: true,
        zone_en_lecture: true,
        played_enough: false,
        duree_ms: DUREE_MS,
        tours: TOURS_D_ARRET_DU_SUIVANT,
        pic_ms: PIC_MS,
        position_ms: 0,
        tenue,
    }
}

#[test]
fn le_depart_de_l_appareil_ne_vaut_que_pour_une_piste_dlna_lachee_en_cours_7700() {
    let notre = TenueDuRenderer::LeNotre;
    assert!(depart_de_l_appareil(&arret(&notre)));
    // Position tenue : rien ne dit que l'appareil a lâché la piste.
    assert!(!depart_de_l_appareil(&ArretRapporte {
        position_ms: PIC_MS,
        ..arret(&notre)
    }));
    // URI vide : il l'a lâchée, même position tenue.
    let vide = TenueDuRenderer::LecteurInterne;
    assert!(depart_de_l_appareil(&ArretRapporte {
        position_ms: PIC_MS,
        ..arret(&vide)
    }));
    let tiers = TenueDuRenderer::AutreApplication;
    assert!(!depart_de_l_appareil(&arret(&tiers)));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        tours: TOURS_D_ARRET_DU_SUIVANT - 1,
        ..arret(&notre)
    }));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        pic_ms: PIC_MINIMAL_MS - 1,
        ..arret(&notre)
    }));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        played_enough: true,
        ..arret(&notre)
    }));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        zone_en_lecture: false,
        ..arret(&notre)
    }));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        output_type: "oaat",
        ..arret(&notre)
    }));
    assert!(!depart_de_l_appareil(&ArretRapporte {
        duree_ms: 0,
        ..arret(&notre)
    }));
}

#[test]
fn seul_un_renderer_qui_avait_de_l_audio_en_main_est_retenu_7700() {
    use le_renderer_a_lache_un_flux_qu_il_avait_en_main as lache;
    let avance = PIC_MS + AVANCE_MINIMALE_MS;
    // Notre URI toujours rapportée : de l'avance, fichier pas servi en entier.
    assert!(lache(Some(avance), PIC_MS, Some(9), Some(10), false));
    assert!(!lache(Some(avance - 1), PIC_MS, Some(9), Some(10), false));
    assert!(!lache(None, PIC_MS, None, None, false));
    // #4661 — fichier servi en entier : le renderer peut jouer son tampon.
    assert!(!lache(Some(avance), PIC_MS, Some(10), Some(10), false));
    // URI vidée : l'appareil a lâché notre flux, entier ou non.
    assert!(lache(None, PIC_MS, Some(10), Some(10), true));
    assert!(lache(Some(avance), PIC_MS, Some(9), Some(10), true));
    assert!(!lache(Some(avance - 1), PIC_MS, Some(9), Some(10), true));
}
