//! Suite de #9175 — de bout en bout, avec une VRAIE sortie Sendspin (pas une
//! factice) : un saut et une pause/reprise sur un flux traité (égaliseur
//! actif, canal qui ne se relit pas) passent par la recréation du flux à la
//! position, et la sortie apprend où ce flux commence
//! (`noter_le_depart_si_le_flux_y_commence` → `noter_le_depart_du_flux`) :
//! elle compte position et durée depuis le début de la piste.
//!
//! L'enceinte est un pilote factice derrière `sendspin::lecteur::relier`, qui
//! accepte chaque ordre. Le flux de banc n'est pas joignable (port 0) : la
//! diffusion échoue, mais la ligne de temps de la sortie, que ce banc lit,
//! est posée avant.

use super::*;
use std::sync::Arc;

use crate::db::models::Track;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::sendspin::{SendspinOutput, TYPE_DE_SORTIE, identifiant_de_sortie};

const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");
const CLIENT: &str = "banc-relance-9175";

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    track_id: i64,
    appareil: String,
}

async fn banc() -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let appareil = identifiant_de_sortie(CLIENT);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create(
            "Sendspin (banc 9175)",
            Some(TYPE_DE_SORTIE),
            Some(&appareil),
        )
        .unwrap();

    // L'enceinte : chaque ordre est accepté.
    let (liaison, mut cote) = crate::sendspin::lecteur::relier(
        vec![crate::sendspin::lecteur::FormatAudio::pcm(44_100, 2, 16)],
        None,
    );
    tokio::spawn(async move {
        while let Some(d) = cote.ordres.recv().await {
            let _ = d.reponse.send(Ok(()));
        }
    });
    orch.outputs
        .lock()
        .await
        .register(Box::new(SendspinOutput::new(
            "Sendspin du banc".into(),
            CLIENT,
            liaison,
        )));

    // L'égaliseur : le flux traité part dans un canal.
    let s = SettingsRepo::with_backend(db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 12.0,
            q: 1.0,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&profil).unwrap(),
    )
    .unwrap();

    let mut t = Track::new("Piste du banc 9175".into());
    t.duration_ms = 1_000;
    t.file_path = Some(FLAC.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(db).create(&t).unwrap();
    Banc {
        orch,
        zone_id,
        track_id,
        appareil,
    }
}

impl Banc {
    async fn jouer(&self) {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(self.appareil.clone()),
                track_id: Some(self.track_id),
                duration_ms: Some(1_000),
                media_format: Some("flac".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                ..Default::default()
            })
            .await;
        assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());
        let sid = self.stream_id().await;
        assert!(
            !self.orch.streamer.is_seekable_session(&sid).await,
            "prémisse : le flux traité est un canal"
        );
    }

    async fn stream_id(&self) -> String {
        self.orch
            .playback
            .get_state(self.zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id)
            .expect("session installée")
    }

    async fn statut(&self) -> crate::outputs::OutputStatus {
        let sortie = self
            .orch
            .outputs
            .lock()
            .await
            .get(&self.appareil)
            .expect("sortie Sendspin enregistrée");
        let s = sortie.lock().await;
        s.get_status().await.unwrap()
    }
}

/// Un saut sur le flux traité : le flux est recréé à 500 ms, et la vraie
/// sortie compte depuis le début de la piste, pas depuis celui du flux neuf.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saut_la_vraie_sortie_compte_depuis_le_debut_de_la_piste_9175() {
    let b = banc().await;
    b.jouer().await;
    let avant = b.stream_id().await;

    let r = b.orch.seek(b.zone_id, 500, Some(&b.appareil)).await;

    assert!(r.is_ok(), "{r:?}");
    assert_ne!(b.stream_id().await, avant, "le flux a été recréé");
    let st = b.statut().await;
    assert!(
        st.position_ms >= 500,
        "suite de #9175 : après la relance à 500 ms, la sortie Sendspin doit compter \
         depuis le début de la piste ({} ms)",
        st.position_ms
    );
    assert_eq!(st.duration_ms, 1_000, "durée de la piste, pas du reste");
}

/// Pause puis reprise sur le flux traité : la sortie dit qu'elle ne peut pas
/// reprendre sur place, la zone rétablit le flux à la position, et la sortie
/// compte depuis le début de la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_reprise_retablit_le_flux_a_la_position_9175() {
    let b = banc().await;
    b.jouer().await;
    b.orch
        .playback
        .play(
            b.zone_id,
            crate::playback::NowPlaying {
                track_id: Some(b.track_id),
                title: "Piste du banc 9175".into(),
                source: "local".into(),
                stream_id: Some(b.stream_id().await),
                duration_ms: 1_000,
                ..Default::default()
            },
        )
        .await;
    b.orch
        .pause(b.zone_id, Some(&b.appareil))
        .await
        .expect("pause");
    // La position que le sondeur aurait relevée.
    b.orch.playback.seek(b.zone_id, 600).await;
    let avant = b.stream_id().await;

    let r = b.orch.resume(b.zone_id, Some(&b.appareil)).await;

    assert!(
        r.is_ok(),
        "suite de #9175 : la reprise d'un canal doit rétablir le flux à la position, \
         pas échouer : {r:?}"
    );
    assert_ne!(b.stream_id().await, avant, "le flux a été rétabli");
    let st = b.statut().await;
    assert!(
        st.position_ms >= 600,
        "la sortie compte depuis le début de la piste ({} ms)",
        st.position_ms
    );
}
