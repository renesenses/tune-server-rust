//! Fil 2125 (#5711, 1.0.0-rc1 Windows, renderer Rygel/GStreamer 1.14.4 en
//! DLNA, FLAC local servi tel quel) : la reprise automatique après décrochage
//! (#4645) repartait du DÉBUT de la piste.
//!
//! Journal, 10:42:17 : `stop`, nouvelle session, `Play`, puis **`Seek` envoyé
//! à 10:42:17.982 et acquitté**, `renderer_cale_reprise_automatique
//! position_ms=194000` à 10:42:18.025 — et **le renderer n'ouvre le flux qu'à
//! 10:42:18.037, avec `range="-"`**, depuis l'octet 0. Le `Seek` était parti
//! avant l'ouverture du flux : acquitté, puis oublié.
//!
//! Le banc : une sortie `dlna` factice qui, comme ce Rygel, ACQUITTE un `Seek`
//! reçu avant d'avoir ouvert le flux mais l'oublie, et n'honore que ceux qui
//! arrivent après son premier GET. Le GET est simulé 55 ms après le `Play`,
//! l'écart mesuré sur le journal.
//!
//! Contre-épreuve : remplacer le corps de
//! `sauter_apres_reprise_de_renderer_cale` par le `self.seek(...)` direct
//! d'avant fait tomber
//! `le_saut_de_reprise_attend_que_le_renderer_ait_ouvert_le_flux`.
use super::PlaybackOrchestrator;
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:uuid-rygel-2125";
/// `peak_pos` de la coupure du fil 2125.
const POSITION_DU_DECROCHAGE_MS: u64 = 194_000;
const DUREE_MS: u64 = 303_573;
/// Écart mesuré entre le `Seek` et l'ouverture du flux sur le journal.
const OUVERTURE_DU_FLUX_MS: u64 = 55;

/// Le Rygel du fil 2125 : un `Seek` reçu avant l'ouverture du flux est
/// acquitté et OUBLIÉ.
struct RygelQuiOublieLeSeekPrecoce {
    flux_ouvert: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    seeks_recus: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl OutputTarget for RygelQuiOublieLeSeekPrecoce {
    fn name(&self) -> &str {
        "Rygel"
    }
    fn device_id(&self) -> &str {
        APPAREIL
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.seeks_recus.fetch_add(1, Ordering::SeqCst);
        if self.flux_ouvert.load(Ordering::SeqCst) {
            self.position_ms.store(position_ms, Ordering::SeqCst);
        }
        // Acquitté dans les deux cas : c'est tout le piège.
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Playing,
            position_ms: self.position_ms.load(Ordering::SeqCst),
            duration_ms: DUREE_MS,
            realtime: true,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    flux_ouvert: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    seeks_recus: Arc<AtomicU64>,
    _scratch: crate::test_scratch::ScratchDir,
}

/// La zone vient d'être relancée par `play_from_queue` (session fichier
/// neuve, `Play` envoyé) : le renderer n'a pas encore ouvert le flux.
async fn zone_juste_relancee() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Rygel", Some("dlna"), Some(APPAREIL))
        .unwrap();
    let flux_ouvert = Arc::new(AtomicBool::new(false));
    let position_ms = Arc::new(AtomicU64::new(0));
    let seeks_recus = Arc::new(AtomicU64::new(0));
    orch.outputs
        .lock()
        .await
        .register(Box::new(RygelQuiOublieLeSeekPrecoce {
            flux_ouvert: flux_ouvert.clone(),
            position_ms: position_ms.clone(),
            seeks_recus: seeks_recus.clone(),
        }));
    let scratch = crate::test_scratch::scratch_dir("tune-reprise-cale-2125");
    let fichier = scratch.join("piste.flac");
    std::fs::write(&fichier, b"fLaC").unwrap();
    let sid = orch
        .streamer
        .create_file_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                file_size: Some(27_200_000),
                ..Default::default()
            },
            fichier.to_string_lossy().into_owned(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Piste du fil 2125".into(),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: DUREE_MS as i64,
                ..Default::default()
            },
        )
        .await;
    Banc {
        orch,
        zone_id,
        flux_ouvert,
        position_ms,
        seeks_recus,
        _scratch: scratch,
    }
}

/// Le GET du renderer, 55 ms après le `Play` — après le `Seek` nu d'avant.
fn ouvrir_le_flux_apres(flux_ouvert: Arc<AtomicBool>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(OUVERTURE_DU_FLUX_MS)).await;
        flux_ouvert.store(true, Ordering::SeqCst);
    });
}

/// LE défaut du fil 2125 : le `Seek` doit arriver APRÈS l'ouverture du flux,
/// sinon le renderer l'acquitte et relit depuis l'octet 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_saut_de_reprise_attend_que_le_renderer_ait_ouvert_le_flux() {
    let b = zone_juste_relancee().await;
    ouvrir_le_flux_apres(b.flux_ouvert.clone());
    b.orch
        .sauter_apres_reprise_de_renderer_cale(b.zone_id, Some(APPAREIL), POSITION_DU_DECROCHAGE_MS)
        .await
        .expect("le saut de reprise ne doit pas échouer");
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
    assert_eq!(
        b.seeks_recus.load(Ordering::SeqCst),
        1,
        "un seul Seek, envoyé une fois le flux ouvert"
    );
    assert_eq!(
        b.position_ms.load(Ordering::SeqCst),
        POSITION_DU_DECROCHAGE_MS,
        "le renderer doit reprendre à 3:14, pas relire depuis le début"
    );
    assert_eq!(
        b.orch.playback.get_state(b.zone_id).await.position_ms,
        POSITION_DU_DECROCHAGE_MS as i64,
        "la position publique suit la reprise"
    );
}

/// Le `Ok` du saut ne prouve rien du renderer : au retour de l'appel, AUCUN
/// `Seek` n'est encore parti. C'est pourquoi le sondeur constate la reprise
/// sur la position mesurée, et non sur cet acquittement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn au_retour_du_saut_aucun_seek_n_est_encore_parti() {
    let b = zone_juste_relancee().await;
    b.orch
        .sauter_apres_reprise_de_renderer_cale(b.zone_id, Some(APPAREIL), POSITION_DU_DECROCHAGE_MS)
        .await
        .expect("le saut de reprise ne doit pas échouer");
    assert_eq!(
        b.seeks_recus.load(Ordering::SeqCst),
        0,
        "le Seek part après la pose, en tâche détachée"
    );
}
