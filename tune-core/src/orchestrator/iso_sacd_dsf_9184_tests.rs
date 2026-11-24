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
    resoudre_image(mode, annonce, ImageFabriquee::deux_pistes(), false).await
}

/// [`resoudre`] sur une image quelconque. `vers_une_image_dst` : la ligne
/// écrite par le scan est ensuite repointée vers une image DST (base
/// d'avant, ajout à la main), que le scan n'aurait pas acceptée.
async fn resoudre_image(
    mode: &str,
    annonce: DsdCapability,
    image: ImageFabriquee,
    vers_une_image_dst: bool,
) -> Resolu {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Kind of Blue.iso");
    let bornes = ecrire(&iso, &image);
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
    let iso = if vers_une_image_dst {
        let dst = dir.path().join("Brothers in Arms.iso");
        let mut image_dst = ImageFabriquee::deux_pistes();
        image_dst.stereo_dst = true;
        ecrire(&dst, &image_dst);
        orch.db
            .execute(
                "UPDATE tracks SET cue_media_path = ? WHERE id = ?",
                &[
                    &dst.to_string_lossy().into_owned() as &dyn crate::db::backend::ToSqlValue,
                    &piste.id.unwrap(),
                ],
            )
            .unwrap();
        dst
    } else {
        iso
    };
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

/// 🔴 #9184 (suite) — la mesure des VU-mètres d'une piste d'ISO servie en
/// DSF virtuel s'arrête à la FIN de la piste dans l'image. Avant la borne,
/// le décodage partait du début de la tranche et courait jusqu'au bout du
/// disque : la piste 1 (3 trames) en mesurait 10.
///
/// Par la fenêtre que choisit le passthrough
/// (`fenetre_des_niveaux_du_passthrough`) et le décodage même du
/// décodage-pour-niveaux (`decoder_pour_les_niveaux`) : on compte le PCM
/// produit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_mesure_des_niveaux_d_une_piste_d_iso_s_arrete_a_sa_fin() {
    use crate::audio::sacd::trames_en_ms;
    use crate::orchestrator::resolve_local::{TrancheCue, fenetre_des_niveaux_du_passthrough};
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Kind of Blue.iso");
    let bornes = ecrire(&iso, &ImageFabriquee::deux_pistes());
    let (debut, fin) = bornes[0];
    let tranche = TrancheCue {
        debut_ms: trames_en_ms(debut),
        duree_ms: Some(trames_en_ms(fin) - trames_en_ms(debut)),
    };
    // Un fichier entier (pas de DSF d'ISO) se mesure de 0 au bout.
    assert_eq!(
        fenetre_des_niveaux_du_passthrough(false, Some(tranche)),
        (0, None)
    );
    let (debut_ms, fin_ms) = fenetre_des_niveaux_du_passthrough(true, Some(tranche));
    assert_eq!(debut_ms, trames_en_ms(debut));

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (relais_tx, _relais_rx) = tokio::sync::mpsc::unbounded_channel();
    let compte = tokio::spawn(async move {
        let mut n = 0usize;
        while let Some(b) = sink_rx.recv().await {
            n += b.len();
        }
        n
    });
    let chemin = iso.to_string_lossy().into_owned();
    let ready = Arc::new(tokio::sync::Notify::new());
    let (profondeur, cadence) = tokio::task::spawn_blocking(move || {
        crate::orchestrator::decoder_pour_les_niveaux(
            &chemin, debut_ms, fin_ms, sink_tx, ready, relais_tx,
        )
    })
    .await
    .unwrap()
    .expect("décodage de la tranche");
    let octets = compte.await.unwrap();
    let par_seconde = cadence as usize * 2 * usize::from(profondeur / 8);
    let mesure_ms = octets * 1000 / par_seconde;
    let attendu_ms = ((fin - debut) * 1000 / 75) as usize;
    assert!(
        mesure_ms.abs_diff(attendu_ms) <= 3,
        "le décodage pour les VU-mètres doit couvrir la PISTE ({attendu_ms} ms), \
         pas le reste du disque : {mesure_ms} ms décodées ({octets} octets à {cadence} Hz)"
    );
}

/// Acquis (#9184) — une image dont la zone lue est MULTICANAL (stéréo DST,
/// multicanal en DSD brut), devant un renderer qui annonce le DSD : la
/// piste reste décodée en PCM, aucun DSF virtuel (l'ordre des voies
/// SACD → DSF n'est pas vérifié).
#[tokio::test]
async fn une_piste_d_iso_multicanal_reste_en_pcm_devant_un_renderer_dsd() {
    let mut image = ImageFabriquee::deux_pistes();
    image.multicanal_dsd = true;
    let r = resoudre_image("auto", annonce_dsf(), image, false).await;
    let resolu = r
        .resolu
        .unwrap_or_else(|e| panic!("la piste multicanal doit se résoudre en PCM : {e}"));
    let m = resolu.mime_type.to_ascii_lowercase();
    assert!(
        !(m.contains("dsd") || m.contains("dsf") || m.contains("dff")),
        "zone multicanal : PCM attendu, servi {m}"
    );
    assert_ne!(resolu.bit_depth, Some(1));
    let sid = resolu.stream_id.clone().unwrap();
    let wire = r.orch.streamer.stream_output_wire(&sid).await.unwrap();
    assert_ne!(wire.format, "dsf");
}

/// Acquis (#9184) — une image DST devant un renderer qui annonce le DSD :
/// refus NOMMÉ, comme aujourd'hui ; jamais un DSF, jamais l'image.
#[tokio::test]
async fn une_piste_d_image_dst_reste_refusee_devant_un_renderer_dsd() {
    let r = resoudre_image("auto", annonce_dsf(), ImageFabriquee::deux_pistes(), true).await;
    let erreur = match r.resolu {
        Err(e) => e,
        Ok(resolu) => panic!(
            "une image DST ne doit pas se résoudre en flux : servi {}",
            resolu.mime_type
        ),
    };
    assert_eq!(
        erreur.strip_prefix("format_not_playable:"),
        Some(crate::audio::sacd::MOTIF_ISO_SACD_DST),
        "refus attendu, nommé : {erreur}"
    );
}
