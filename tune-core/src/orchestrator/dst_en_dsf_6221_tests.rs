//! 🔴 #6221 — un DSDIFF DST vers un renderer DLNA qui annonce le DSD part en
//! DSD NATIF (DSF fabriqué à la volée) ; vers un renderer sans DSD, en PCM
//! comme avant.
//!
//! Par la porte publique (`resolve_stream`), zone DLNA en mode DSD « auto » :
//! la décision dépend alors de ce qu'annonce le renderer (GetProtocolInfo,
//! mis en cache dans `dsd_capabilities`), exactement comme pour un `.dsf`
//! non compressé servi tel quel.
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
use crate::outputs::dlna::DsdCapability;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const RENDERER: &str = "dlna-6221";
const FIXTURE: &str = "dst_fate_dsd64_stereo.dff";

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

struct Resolu {
    orch: PlaybackOrchestrator,
    resolu: Result<ResolvedStream, String>,
    #[cfg_attr(not(feature = "dst"), allow(dead_code))]
    fichier: String,
    _dir: tempfile::TempDir,
}

/// Zone DLNA en « auto » ; `annonce` est ce que le Sink du renderer dit du
/// DSD (le cache que remplit `parse_dsd_capability`).
async fn resoudre(annonce: DsdCapability) -> Resolu {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste-6221.dff");
    std::fs::copy(
        format!(
            "{}/tests/fixtures/dsd/{FIXTURE}",
            env!("CARGO_MANIFEST_DIR")
        ),
        &chemin,
    )
    .unwrap();
    let fichier = chemin.to_string_lossy().into_owned();
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, file_path, format, duration_ms, sample_rate, \
             bit_depth, channels) VALUES (1, 'Piste', ?, 'dff', 133, 2822400, 1, 2)",
            &[&fichier as &dyn ToSqlValue],
        )
        .unwrap();
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones.create("Salon", Some("dlna"), Some(RENDERER)).unwrap();
    zones.update_dsd_mode(zone_id, "auto").unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(RENDERER, "Salon").with_type("dlna"),
    ));
    orch.dsd_capabilities
        .lock()
        .await
        .insert(RENDERER.to_string(), annonce);
    let resolu = orch
        .resolve_stream(&PlayRequest {
            zone_id,
            output_device_id: Some(RENDERER.into()),
            track_id: Some(1),
            source: Some("local".into()),
            ..Default::default()
        })
        .await;
    Resolu {
        orch,
        resolu,
        fichier,
        _dir: dir,
    }
}

async fn session(
    orch: &PlaybackOrchestrator,
    sid: &str,
) -> Arc<crate::http::streamer::StreamSession> {
    orch.streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session créée")
}

/// 🔴 Le renderer annonce le DSF : le DST part en DSD natif, sous le MIME
/// que le renderer a annoncé (la même règle que le passthrough d'un `.dsf`),
/// avec la taille EXACTE du DSF fabriqué, et le fil le dit au chemin du
/// signal.
#[cfg(feature = "dst")]
#[tokio::test]
async fn un_dst_vers_un_renderer_dsd_part_en_dsf_natif() {
    let r = resoudre(DsdCapability {
        supports_dsf: true,
        supports_dff: false,
        dsf_mime: Some("audio/x-dsf".into()),
    })
    .await;
    let resolu = r.resolu.expect("le DST se résout");
    assert_eq!(
        resolu.mime_type, "audio/x-dsf",
        "DSD natif attendu, sous le MIME du renderer — servi : {} / {:?} Hz / {:?} bits",
        resolu.mime_type, resolu.sample_rate, resolu.bit_depth
    );
    assert_eq!(resolu.sample_rate, Some(2_822_400));
    assert_eq!(resolu.bit_depth, Some(1));
    let attendu = crate::audio::dsf_depuis_dst::DsfDepuisDst::preparer(&r.fichier).unwrap();
    assert_eq!(
        resolu.file_size,
        Some(attendu.total()),
        "res@size = DSF fabriqué"
    );
    let sid = resolu.stream_id.clone().unwrap();
    let wire = r.orch.streamer.stream_output_wire(&sid).await.unwrap();
    assert_eq!(wire.format, "dsf");
    assert!(wire.dsd_reconstruit_depuis_dst);
    let s = session(&r.orch, &sid).await;
    let v = s
        .source_virtuelle
        .lock()
        .unwrap()
        .clone()
        .expect("la session sert le DSF virtuel, pas le .dff DST");
    assert_eq!(v.total(), attendu.total());
    let mut tete = Vec::new();
    v.lire(0, 92, &mut |m| {
        tete.extend_from_slice(&m);
        Ok(())
    })
    .unwrap();
    assert_eq!(&tete[..], &attendu.entete()[..]);
}

/// 🔴 Le renderer n'annonce PAS le DSD : comportement d'avant, le DST est
/// décodé en PCM (WAV), aucun fichier virtuel.
#[tokio::test]
async fn un_dst_vers_un_renderer_sans_dsd_reste_en_pcm() {
    let r = resoudre(DsdCapability {
        supports_dsf: false,
        supports_dff: false,
        dsf_mime: None,
    })
    .await;
    match r.resolu {
        Ok(resolu) => {
            let m = resolu.mime_type.to_ascii_lowercase();
            assert!(
                !(m.contains("dsd") || m.contains("dsf") || m.contains("dff")),
                "renderer sans DSD : PCM attendu, servi {m}"
            );
            assert_ne!(resolu.bit_depth, Some(1));
            assert_eq!(resolu.sample_rate, Some(176_400), "DSD64 → PCM 176,4 kHz");
            let sid = resolu.stream_id.clone().unwrap();
            let s = session(&r.orch, &sid).await;
            assert!(s.source_virtuelle.lock().unwrap().is_none());
        }
        Err(e) => {
            if cfg!(feature = "dst") {
                panic!("le DST doit se lire : {e}");
            }
        }
    }
}
