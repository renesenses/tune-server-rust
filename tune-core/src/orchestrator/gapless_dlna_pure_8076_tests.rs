//! #8076 — The Wall, pistes 1 → 2, vers l'Eversolo DMP-A8 (DLNA) en PURE.
//!
//! Le journal du .18 portait `gapless_next_set … streaming=true` puis
//! `gapless_levels_after_advance … decision=decodage_du_fichier`, lus comme
//! « FLAC décodé puis ré-encodé ». Ce n'était pas le cas : la session servait
//! les trames du fichier (`flac_conteneur_neuf_en_passthrough`, 51 915 398
//! octets servis sur 51 915 398, `complet=true`), et `decodage_du_fichier`
//! désigne le décodage qui anime les VU-mètres (`queue.rs`, branche
//! `fichier_a_mesurer_apres_avance`).
//!
//! Ces témoins fixent les trois faits sur lesquels le diagnostic repose :
//! 1. un FLAC local vers une zone DLNA en PURE part TEL QUEL, et la session
//!    le dit (`voie = fichier_tel_quel`) ;
//! 2. un FLAC écrit par ffmpeg sans MD5 (la forme des fichiers de The Wall)
//!    part sous un en-tête neuf, trames intactes : mêmes échantillons, même
//!    nombre, que le fichier source ;
//! 3. le FLAC RÉ-ENCODÉ par Tune (voie prise quand un traitement est actif)
//!    garde exactement le nombre d'échantillons de la source : STREAMINFO
//!    exact, aucune trame bourrée, aucun échantillon perdu.
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::voie;
use crate::orchestrator::PlaybackOrchestrator;
use std::sync::Arc;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(nom: &str) -> String {
    format!("{FIXTURES}/{nom}")
}

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

/// Une zone DLNA en PURE. L'appareil est inconnu du registre : comme en
/// production sans sonde, il est réputé lire le FLAC.
fn zone_dlna_pure(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("DMP-A8", Some("dlna"), Some("uuid:dmp-a8-banc-8076"))
        .unwrap();
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert!(crate::audio::audiophile::zone_enabled(&orch.db, zone_id));
    zone_id
}

fn piste(orch: &PlaybackOrchestrator, chemin: &str, cadence: i32, profondeur: i32) -> i64 {
    let mut t = crate::db::models::Track::new("The Thin Ice (banc)".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(cadence);
    t.bit_depth = Some(profondeur);
    t.channels = 2;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&t)
        .unwrap()
}

async fn resoudre(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
    track_id: i64,
) -> crate::orchestrator::ResolvedQueueItem {
    PlayQueueRepo::with_backend(orch.db.clone())
        .append(zone_id, &[QueueInput::Local { track_id }])
        .unwrap();
    orch.resolve_queue_item_url(zone_id, 0).await.unwrap()
}

/// Nombre total d'échantillons (par canal) et MD5 que le STREAMINFO énonce.
fn streaminfo(octets: &[u8]) -> (u64, [u8; 16]) {
    assert_eq!(&octets[..4], b"fLaC");
    assert_eq!(octets[4] & 0x7f, 0, "STREAMINFO d'abord");
    let si = &octets[8..42];
    let mot = u64::from_be_bytes(si[10..18].try_into().unwrap());
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&si[18..34]);
    (mot & ((1 << 36) - 1), md5)
}

/// PCM décodé (entrelacé) et nombre de trames inter-canaux.
fn decoder(chemin: &str) -> (Vec<i32>, u64) {
    let d = crate::audio::decode::decode_to_pcm(chemin, None, None, 0.0, 0.0).unwrap();
    let trames = d.samples_i32.len() as u64 / u64::from(d.channels);
    (d.samples_i32, trames)
}

/// Fait 1 — un FLAC de référence (MD5 réel), 96/24 comme The Wall, vers une
/// zone DLNA en PURE : passthrough, et la voie le dit.
#[tokio::test]
async fn un_flac_local_vers_une_zone_dlna_en_pure_part_tel_quel_8076() {
    let orch = orchestrateur();
    let zone_id = zone_dlna_pure(&orch);
    let chemin = fixture("flac/ref_24_96000_stereo.flac");
    let id = piste(&orch, &chemin, 96_000, 24);

    let r = resoudre(&orch, zone_id, id).await;

    assert_eq!(r.mime_type, "audio/flac");
    let sid = r.stream_id.expect("session de fichier");
    assert_eq!(
        orch.streamer.voie_servie(&sid).await,
        voie::FICHIER_TEL_QUEL,
        "PURE, FLAC lu par le renderer : le fichier part octet pour octet"
    );
    assert_eq!(
        r.file_size,
        std::fs::metadata(&chemin).ok().map(|m| m.len()),
        "Content-Length = taille du fichier sur disque"
    );
    assert_eq!(r.sample_rate, Some(96_000));
    assert_eq!(r.bit_depth, Some(24));
}

/// Fait 2 — un FLAC `Lavf` à MD5 nul (#4350), la forme des fichiers de The
/// Wall sur le .18 : en-tête neuf, trames intactes. Les octets servis sont
/// reconstruits par la carte que la session porte, exactement comme la
/// branche fichier de `tune-stream-http` les sert ; décodés, ils rendent les
/// mêmes échantillons que la source, ni plus ni moins.
#[tokio::test]
async fn un_flac_ffmpeg_sans_md5_garde_ses_trames_et_ses_echantillons_8076() {
    let bac = crate::test_scratch::scratch_dir("tune-8076-lavf");
    let source = bac.join("02 - piste.flac");
    let mut octets = std::fs::read(fixture("test.flac")).unwrap();
    // MD5 de STREAMINFO (octets 26..42) mis à zéro : vendeur `Lavf` + MD5
    // nul, la combinaison de #4350.
    octets[26..42].fill(0);
    std::fs::write(&source, &octets).unwrap();
    let source = source.to_string_lossy().into_owned();
    assert!(crate::audio::flac_vendeur::flac_ecrit_par_ffmpeg(
        std::path::Path::new(&source)
    ));

    let orch = orchestrateur();
    let zone_id = zone_dlna_pure(&orch);
    let id = piste(&orch, &source, 44_100, 16);
    let r = resoudre(&orch, zone_id, id).await;

    assert_eq!(r.mime_type, "audio/flac");
    let sid = r.stream_id.expect("session de fichier");
    assert_eq!(
        orch.streamer.voie_servie(&sid).await,
        voie::ENTETE_FLAC_NEUF,
        "en-tête réécrit, rien de décodé"
    );
    let carte = {
        let sessions = orch.streamer.sessions_state();
        let s = sessions.lock().await;
        s.get(&sid)
            .unwrap()
            .faststart
            .lock()
            .unwrap()
            .clone()
            .expect("carte d'en-tête posée")
    };
    assert_eq!(
        r.file_size,
        Some(carte.total),
        "Content-Length = octets servis"
    );

    let mut servi = carte.header.clone();
    let debut = usize::try_from(carte.body_src_start).unwrap();
    servi.extend_from_slice(&octets[debut..]);
    assert_eq!(servi.len() as u64, carte.total);
    assert_eq!(
        &servi[carte.header.len()..],
        &octets[debut..],
        "les trames partent à l'octet près"
    );
    let (total_source, _) = streaminfo(&octets);
    let (total_servi, _) = streaminfo(&servi);
    assert_eq!(total_servi, total_source, "STREAMINFO recopié");

    let chemin_servi = bac.join("servi.flac");
    std::fs::write(&chemin_servi, &servi).unwrap();
    let (pcm_source, n_source) = decoder(&source);
    let (pcm_servi, n_servi) = decoder(&chemin_servi.to_string_lossy());
    assert_eq!(n_source, total_source, "la source se décode en entier");
    assert_eq!(n_servi, total_source, "aucun échantillon perdu ni ajouté");
    assert!(pcm_servi == pcm_source, "échantillons identiques");
}

/// Fait 3 — le FLAC ré-encodé par Tune (chemin d'un traitement actif sur une
/// zone réseau) : nombre total d'échantillons exact dans STREAMINFO, MD5 nul
/// (valeur « inconnu » permise par la norme), dernière trame plus courte,
/// jamais bourrée, et le même nombre d'échantillons au décodage que la
/// source. Sans traitement, les échantillons eux-mêmes sont identiques.
#[tokio::test]
async fn le_flac_reencode_garde_exactement_le_nombre_d_echantillons_8076() {
    for (nom, cadence, canaux, profondeur) in [
        ("flac/ref_16_44100_stereo.flac", 44_100u32, 2u16, 16u16),
        ("flac/ref_24_96000_stereo.flac", 96_000, 2, 24),
        ("test.flac", 44_100, 2, 16),
    ] {
        let src = fixture(nom);
        let (total_source, _) = streaminfo(&std::fs::read(&src).unwrap());
        let (pcm_source, _) = decoder(&src);
        for gain in [None, Some(0.5)] {
            let bac = crate::test_scratch::scratch_dir("tune-8076-reencode");
            let dest = bac.join("converti.flac").to_string_lossy().into_owned();
            super::transcodage::transcode_source_to_file_avec_crossfeed(
                src.clone(),
                cadence,
                canaux,
                profondeur,
                "flac".into(),
                None,
                None,
                gain,
                dest.clone(),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let converti = std::fs::read(&dest).unwrap();
            let (total, md5) = streaminfo(&converti);
            assert_eq!(total, total_source, "{nom} {gain:?} : STREAMINFO exact");
            assert_eq!(md5, [0u8; 16], "{nom} : MD5 non calculé (permis)");
            let (pcm, n) = decoder(&dest);
            assert_eq!(
                n, total_source,
                "{nom} {gain:?} : rien de perdu, rien d'ajouté"
            );
            if gain.is_none() {
                assert!(pcm == pcm_source, "{nom} : échantillons identiques");
            }
        }
    }
}

/// La voie d'une session que personne n'a notée se déduit de sa forme.
#[test]
fn la_voie_non_notee_se_deduit_de_la_session_8076() {
    assert_eq!(voie::classer(None, true, false, false), voie::FICHIER);
    assert_eq!(voie::classer(None, false, true, false), voie::MANDATAIRE);
    assert_eq!(voie::classer(None, false, false, true), voie::RADIO);
    assert_eq!(
        voie::classer(None, false, false, false),
        voie::FLUX_CONVERTI
    );
    assert_eq!(
        voie::classer(Some(voie::FICHIER_TEL_QUEL), true, false, false),
        voie::FICHIER_TEL_QUEL
    );
}
