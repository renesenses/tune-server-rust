//! #9117 (Pingalim, fils 2927/2937, rc12 Linux) — un DSF servi TEL QUEL à un
//! renderer DLNA qui annonce le DSD (`dsd_mode=auto`,
//! `dsd_passthrough_decide … annonce_du_renderer`) laissait VU-mètres et
//! spectre immobiles : le décodage-pour-niveaux du passthrough saute le DSD
//! (`niveaux_en_passthrough_a_sauter`), et rien d'autre ne décodait.
//!
//! Le banc passe par la VRAIE porte de lecture (`PlaybackOrchestrator::play`),
//! sur une zone `dlna` dont le renderer « annonce » le DSD
//! (`retenir_annonce_dsd_pour_banc`), et compte les `playback.audio_levels`
//! publiés sur le bus. Aucun son, aucun périphérique : le registre des sorties
//! est vide, l'envoi échoue après la résolution, qui seule arme les niveaux.
//!
//! Ce qui prouve que le banc mesure : la contre-épreuve (bloc #9117 de
//! `resolve_local.rs` neutralisé → 0 trame, test rouge). Un témoin « même
//! DSF en mode `pcm` » a été écarté : sur ce banc, où aucun renderer ne tire
//! le flux, le chemin transcodé ne publie aucune trame.
use crate::db::backend::DbBackend;
use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::{EventBus, TuneEvent};
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::dlna::DsdCapability;
use crate::playback::{NowPlaying, PlaybackManager};
use std::sync::Arc;

const RENDERER: &str = "uuid:renderer-dsd-9117";
const DUREE_MS: i64 = 4_000;

struct Banc {
    orch: PlaybackOrchestrator,
    playback: Arc<PlaybackManager>,
    rx: tokio::sync::broadcast::Receiver<TuneEvent>,
    zone_id: i64,
    track_id: i64,
    _dossier: tempfile::TempDir,
}

async fn banc(dsd_mode: &str) -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let bus = Arc::new(EventBus::new());
    let rx = bus.subscribe();
    orch.event_bus = Some(bus);
    orch.retenir_annonce_dsd_pour_banc(
        RENDERER,
        DsdCapability {
            supports_dsf: true,
            supports_dff: true,
            dsf_mime: Some("audio/x-dsf".into()),
            ..Default::default()
        },
    )
    .await;

    let zones = ZoneRepo::with_backend(db.clone());
    let zone_id = zones
        .create("Salon (banc 9117)", Some("dlna"), Some(RENDERER))
        .unwrap();
    zones.update_dsd_mode(zone_id, dsd_mode).unwrap();

    let dossier = tempfile::tempdir().unwrap();
    let dsf = dossier.path().join("piste-9117.dsf");
    crate::audio::decode::dop_deplacement_tests_6456::ecrire_dsf(
        &dsf,
        4 * crate::audio::decode::dop_deplacement_tests_6456::OCTETS_PAR_SECONDE,
    );
    let mut t = Track::new("Piste 9117 (DSF)".into());
    t.duration_ms = DUREE_MS;
    t.file_path = Some(dsf.to_string_lossy().into_owned());
    t.format = Some("dsf".into());
    t.sample_rate = Some(2_822_400);
    t.bit_depth = Some(1);
    t.channels = 2;
    t.file_size = std::fs::metadata(&dsf).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    Banc {
        orch,
        playback,
        rx,
        zone_id,
        track_id,
        _dossier: dossier,
    }
}

impl Banc {
    /// `PlaybackOrchestrator::play`, puis la zone marquée `Playing` comme le
    /// ferait `transport.rs` une fois l'ordre parti (le forwarder gèle son
    /// horloge tant que la zone ne joue pas).
    async fn jouer(&self) -> String {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(RENDERER.to_string()),
                track_id: Some(self.track_id),
                duration_ms: Some(DUREE_MS),
                media_format: Some("dsf".into()),
                sample_rate: Some(2_822_400),
                bit_depth: Some(1),
                ..Default::default()
            })
            .await;
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    title: "Piste 9117 (DSF)".into(),
                    source: "library".into(),
                    format: Some("dsf".into()),
                    duration_ms: DUREE_MS,
                    ..Default::default()
                },
            )
            .await;
        r.ok().and_then(|r| r.stream_url).unwrap_or_default()
    }

    /// Nombre de trames de niveaux de CETTE zone et crête maximale vue.
    async fn compter(&mut self, attendus: u32) -> (u32, f64) {
        let (mut n, mut crete) = (0u32, f64::NEG_INFINITY);
        let echeance = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while n < attendus {
            let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() {
                break;
            }
            match tokio::time::timeout(reste, self.rx.recv()).await {
                Ok(Ok(ev))
                    if ev.event_type == "playback.audio_levels"
                        && ev.data["zone_id"].as_i64() == Some(self.zone_id) =>
                {
                    n += 1;
                    if let Some(p) = ev.data["peak_left_db"].as_f64() {
                        crete = crete.max(p);
                    }
                }
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => break,
            }
        }
        (n, crete)
    }
}

const ATTENDUS: u32 = 6;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dsf_servi_tel_quel_a_un_renderer_dlna_publie_des_niveaux_9117() {
    let mut b = banc("auto").await;
    let url = b.jouer().await;
    assert!(
        url.ends_with(".dsf"),
        "le banc doit mesurer le PASSTHROUGH DSD (DSF servi tel quel) : URL {url:?}"
    );
    let (n, crete) = b.compter(ATTENDUS).await;
    assert!(
        n >= ATTENDUS,
        "#9117 : un DSF servi tel quel à un renderer DLNA ne publie que {n} \
         trame(s) de niveaux (attendu ≥ {ATTENDUS}) — VU-mètres et spectre \
         restent immobiles"
    );
    assert!(
        crete > -90.0,
        "les niveaux mesurent la source : crête {crete}"
    );
}
