//! 🔴 #4378 — un renderer DLNA ne reçoit JAMAIS un DSDIFF compressé DST brut.
//!
//! Le passthrough DSD d'une zone réseau sert le `.dsf`/`.dff` tel quel quand
//! la zone est réglée en « natif » (ou en « auto » sur un renderer qui annonce
//! le DSD). Or un `.dff` DST n'est pas du DSD : ses trames sont un code
//! entropique, et un renderer qui les lit comme des bits DSD rend du bruit.
//!
//! Le témoin passe par la porte publique (`resolve_stream`), zone DLNA réglée
//! en « natif » — le réglage qui envoie le brut SANS sonder le renderer. Son
//! jumeau, un `.dff` en DSD non compressé, prouve que la mesure voit bien le
//! passthrough : sans lui, un « pas de DSD sur le fil » pourrait venir d'une
//! zone qui ne fait jamais de passthrough.
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const RENDERER: &str = "dlna-4378";

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

/// Une zone DLNA en DSD « natif », et la piste locale `fixture` copiée sous un
/// nom en `.dff`.
async fn resoudre(fixture: &str) -> (Result<ResolvedStream, String>, tempfile::TempDir) {
    let (_orch, resolu, dir) = resoudre_avec(fixture).await;
    (resolu, dir)
}
/// Comme [`resoudre`], en rendant aussi l'orchestrateur (ses sessions).
async fn resoudre_avec(
    fixture: &str,
) -> (
    PlaybackOrchestrator,
    Result<ResolvedStream, String>,
    tempfile::TempDir,
) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste-4378.dff");
    std::fs::copy(
        format!(
            "{}/tests/fixtures/dsd/{fixture}",
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
    zones.update_dsd_mode(zone_id, "native").unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(RENDERER, "Salon").with_type("dlna"),
    ));
    let resolu = orch
        .resolve_stream(&PlayRequest {
            zone_id,
            output_device_id: Some(RENDERER.into()),
            track_id: Some(1),
            source: Some("local".into()),
            ..Default::default()
        })
        .await;
    (orch, resolu, dir)
}

fn est_du_dsd(resolu: &ResolvedStream) -> bool {
    let m = resolu.mime_type.to_ascii_lowercase();
    m.contains("dsd") || m.contains("dsf") || m.contains("dff")
}

/// Le jumeau : un `.dff` en DSD non compressé part BRUT au renderer « natif ».
#[tokio::test]
async fn un_dff_dsd_brut_part_en_passthrough_vers_un_renderer_natif() {
    let (resolu, _dir) = resoudre("ref_dsd64_stereo.dff").await;
    let resolu = resolu.expect("un DFF en DSD brut se résout");
    assert!(
        est_du_dsd(&resolu),
        "zone « natif » : le DSD brut doit partir tel quel (sinon le témoin \
         DST ne mesure rien) — servi : {} ({:?} bits)",
        resolu.mime_type,
        resolu.bit_depth
    );
}

/// 🔴 #4378 — le même réglage, un `.dff` DST : le renderer ne reçoit JAMAIS
/// le fichier DST. Depuis #6221, la zone « natif » reçoit un DSF FABRIQUÉ
/// depuis le DST décodé (fichier virtuel de la session) ; sans la feature
/// `dst`, rien ne décode le DST et un refus est la seule issue juste.
#[tokio::test]
async fn un_dff_dst_n_est_jamais_servi_brut_a_un_renderer() {
    let (orch, resolu, _dir) = resoudre_avec("dst_fate_dsd64_stereo.dff").await;
    match resolu {
        Ok(resolu) => {
            let sid = resolu.stream_id.clone().expect("session");
            let session = orch
                .streamer
                .sessions_state()
                .lock()
                .await
                .get(&sid)
                .cloned()
                .expect("session créée");
            let virtuelle = session.source_virtuelle.lock().unwrap().clone();
            if est_du_dsd(&resolu) {
                // Du DSD sur le fil : ce doit être le DSF fabriqué, jamais
                // les octets du `.dff` DST.
                let v = virtuelle.expect(
                    "un DSDIFF DST est parti BRUT vers le renderer : il lirait \
                     des trames DST comme du DSD, donc du bruit",
                );
                let mut tete = Vec::new();
                v.lire(0, 4, &mut |m| {
                    tete.extend_from_slice(&m);
                    Ok(())
                })
                .unwrap();
                assert_eq!(&tete, b"DSD ", "le fil porte un DSF, pas le FRM8 DST");
            } else {
                assert_ne!(resolu.bit_depth, Some(1));
            }
        }
        Err(e) => assert!(
            cfg!(not(feature = "dst")),
            "avec la feature `dst`, un DSDIFF DST doit se résoudre : {e}"
        ),
    }
}
