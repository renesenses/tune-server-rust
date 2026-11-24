//! 🔴 #9184 (fil 2957) — une piste d'ISO SACD vers un renderer DLNA qui lit
//! le DSD part en DSD NATIF, sous la forme d'un DSF virtuel bâti sur la
//! TRANCHE de l'image ; en mode « pcm », ou vers un renderer sans DSD, elle
//! reste décodée en PCM comme avant.
//!
//! Par la porte publique (`resolve_stream`), avec des pistes écrites par
//! l'écrivain du scan lui-même (`ecrire_les_iso_sacd`) sur une image
//! fabriquée (`sacd::fabrique`) dont le DSD est CONNU.
//!
//! Contre-épreuve : sans le correctif, `est_une_tranche_cue` force le
//! décodage et `une_piste_d_iso_vers_un_renderer_dsd_part_en_dsf_natif`
//! reçoit du WAV.
use crate::audio::dsf_depuis_dst::{BLOC_DSF, ENTETE_DSF_LEN};
use crate::audio::dsf_depuis_iso::DsfDepuisIso;
use crate::audio::sacd::fabrique::{ImageFabriquee, ecrire, octets_attendus};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
use crate::outputs::dlna::DsdCapability;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const RENDERER: &str = "dlna-9184";

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
    iso: std::path::PathBuf,
    /// Trames `[début, fin)` de la piste jouée (la 2e de l'image).
    tranche: (u32, u32),
    _dir: tempfile::TempDir,
}

/// La piste 2 de l'image, sur une zone DLNA réglée `mode`, devant un
/// renderer qui annonce `annonce`.
async fn resoudre(mode: &str, annonce: DsdCapability) -> Resolu {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Kind of Blue.iso");
    let bornes = ecrire(&iso, &ImageFabriquee::deux_pistes());
    let isos = vec![crate::audio::sacd::IsoSacdLu {
        chemin: iso.clone(),
        disque: crate::audio::sacd::lire_disque(&iso).unwrap(),
    }];
    let mut bilan = crate::scanner::cue_bibliotheque::BilanCue::default();
    crate::scanner::cue_bibliotheque::ecrire_les_iso_sacd(&orch.db, &isos, &mut bilan);
    let tranche = bornes[1];
    let piste = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .get_by_cue_identity(
            &iso.to_string_lossy(),
            crate::audio::sacd::trames_en_ms(tranche.0) as i64,
        )
        .unwrap()
        .expect("piste 2 écrite par le scan");
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones.create("Salon", Some("dlna"), Some(RENDERER)).unwrap();
    zones.update_dsd_mode(zone_id, mode).unwrap();
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
            track_id: piste.id,
            source: Some("local".into()),
            ..Default::default()
        })
        .await;
    Resolu {
        orch,
        resolu,
        iso,
        tranche,
        _dir: dir,
    }
}

fn annonce_dsf() -> DsdCapability {
    DsdCapability {
        supports_dsf: true,
        supports_dff: false,
        dsf_mime: Some("audio/x-dsf".into()),
        ..Default::default()
    }
}

fn sans_dsd() -> DsdCapability {
    DsdCapability {
        supports_dsf: false,
        supports_dff: false,
        dsf_mime: None,
        ..Default::default()
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

fn lire(v: &dyn crate::http::streamer::SourceVirtuelle, debut: u64, longueur: u64) -> Vec<u8> {
    let mut o = Vec::new();
    v.lire(debut, longueur, &mut |m| {
        o.extend_from_slice(&m);
        Ok(())
    })
    .unwrap();
    o
}

/// 🔴 Le renderer annonce le DSF (modes « auto » et « native ») : la piste
/// d'ISO part en DSD natif, sous le MIME du renderer, avec la taille EXACTE
/// du DSF de la tranche ; les octets servis sont un DSF valide dont les
/// données sont le DSD de la TRANCHE (bits inversés, blocs de 4 096 octets
/// par canal) ; une fenêtre `Range` rend les mêmes octets que le tout.
#[tokio::test]
async fn une_piste_d_iso_vers_un_renderer_dsd_part_en_dsf_natif() {
    for mode in ["auto", "native"] {
        let r = resoudre(mode, annonce_dsf()).await;
        let resolu = r.resolu.expect("la piste d'ISO se résout");
        assert_eq!(
            resolu.mime_type, "audio/x-dsf",
            "mode {mode} : DSD natif attendu — servi {} / {:?} Hz / {:?} bits",
            resolu.mime_type, resolu.sample_rate, resolu.bit_depth
        );
        assert_eq!(resolu.sample_rate, Some(2_822_400));
        assert_eq!(resolu.bit_depth, Some(1));
        let (debut, fin) = r.tranche;
        let attendu = DsfDepuisIso::preparer(
            &r.iso,
            crate::audio::sacd::trames_en_ms(debut),
            Some(crate::audio::sacd::trames_en_ms(fin)),
        )
        .unwrap();
        assert_eq!(
            resolu.file_size,
            Some(attendu.total()),
            "res@size = DSF de la tranche"
        );
        let sid = resolu.stream_id.clone().unwrap();
        let wire = r.orch.streamer.stream_output_wire(&sid).await.unwrap();
        assert_eq!(wire.format, "dsf");
        assert_eq!(wire.file_size, Some(attendu.total()), "Content-Length");
        let s = session(&r.orch, &sid).await;
        let v = s
            .source_virtuelle
            .lock()
            .unwrap()
            .clone()
            .expect("la session sert le DSF virtuel, jamais l'image");
        assert_eq!(v.total(), attendu.total());

        let fichier = lire(v.as_ref(), 0, v.total());
        assert_eq!(fichier.len() as u64, attendu.total());
        let info = crate::audio::dsf::parse_dsf_from_bytes(&fichier).expect("DSF valide");
        assert_eq!((info.channels, info.sample_rate), (2, 2_822_400));
        let par_canal = u64::from(fin - debut) * 4704;
        assert_eq!(
            info.total_samples,
            par_canal * 8,
            "échantillons de la tranche"
        );
        let donnees = &fichier[ENTETE_DSF_LEN as usize..];
        let mut dsd = Vec::new();
        for i in 0..par_canal as usize {
            let (bloc, dans) = (i / BLOC_DSF, i % BLOC_DSF);
            for canal in 0..2 {
                dsd.push(donnees[bloc * BLOC_DSF * 2 + canal * BLOC_DSF + dans].reverse_bits());
            }
        }
        assert!(
            dsd == octets_attendus(debut, fin),
            "mode {mode} : les données servies ne sont pas le DSD de la tranche"
        );
        // Range : un saut au 3e bloc rend les octets correspondants.
        let a = ENTETE_DSF_LEN + 2 * (BLOC_DSF as u64) * 2;
        let partie = lire(v.as_ref(), a, 5000);
        assert!(partie[..] == fichier[a as usize..a as usize + 5000]);
    }
}

/// 🔴 Repli PCM : zone réglée « pcm » devant un renderer DSD, ou renderer
/// sans DSD en « auto » — la tranche est décodée en PCM comme avant, aucun
/// fichier virtuel.
#[tokio::test]
async fn une_piste_d_iso_reste_en_pcm_si_la_zone_ou_le_renderer_le_veut() {
    for (mode, annonce) in [("pcm", annonce_dsf()), ("auto", sans_dsd())] {
        let r = resoudre(mode, annonce).await;
        let resolu = r
            .resolu
            .unwrap_or_else(|e| panic!("mode {mode} : la piste doit se résoudre : {e}"));
        let m = resolu.mime_type.to_ascii_lowercase();
        assert!(
            !(m.contains("dsd") || m.contains("dsf") || m.contains("dff")),
            "mode {mode} : PCM attendu, servi {m}"
        );
        assert_ne!(resolu.bit_depth, Some(1));
        let sid = resolu.stream_id.clone().unwrap();
        let wire = r.orch.streamer.stream_output_wire(&sid).await.unwrap();
        assert_ne!(wire.format, "dsf");
    }
}
