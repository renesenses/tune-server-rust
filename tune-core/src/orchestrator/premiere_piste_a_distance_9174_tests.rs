//! #9174 (suite) — la PREMIÈRE piste d'une zone navigateur tirée à distance
//! part en FLAC, et les index du remux et du ré-encodage sont justes sur une
//! piste longue.
//!
//! De bout en bout dans `tune-core` : un faux TIDAL (FLAC CD en DASH, banc de
//! #7266), une zone navigateur, `play()`. La provenance est notée AVANT la
//! préparation, comme le fait la couche du serveur sur un `POST …/play`
//! relayé ; la piste servie est du FLAC. La même zone, en local, reçoit du
//! WAV.

use std::sync::Arc;

use super::tidal_cd_dash_jusqu_au_play_7266::{
    PISTE, SECONDES, decouper, faux_tidal, flac_cd, pcm_cd, segment_init, segment_media,
    trames_flac,
};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::audio::seektable_flac_9174::{blocs_de_metadonnees, ecrire_flac_indexe};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::streaming::tidal::TidalService;

/// L'URL du flux que `play()` rend pour une zone navigateur, provenance
/// notée (ou non) AVANT la lecture.
async fn premiere_piste(a_distance: bool) -> String {
    let flac = flac_cd(&pcm_cd()).await;
    let piste = Arc::new(decouper(&flac));
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        Some("127.0.0.1".into()),
    );
    let (base, _journal) = faux_tidal(piste, "flac").await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce téléphone", Some("browser"), None)
        .unwrap();
    if a_distance {
        orch.noter_provenance_de_zone(zone_id, true);
    }
    let r = orch
        .play(PlayRequest {
            zone_id,
            source: Some("tidal".into()),
            source_id: Some(PISTE.into()),
            title: Some("Témoin 9174".into()),
            artist_name: Some("Banc".into()),
            album_title: Some("DASH".into()),
            duration_ms: Some(SECONDES as i64 * 1000),
            ..Default::default()
        })
        .await
        .expect("la lecture part");
    r.stream_url
        .expect("une zone navigateur reçoit l'URL du flux")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_premiere_piste_demandee_par_le_relais_part_en_flac() {
    let url = premiere_piste(true).await;
    assert!(
        url.ends_with(".flac"),
        "première piste d'une zone tirée à distance : {url}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_meme_premiere_piste_en_local_part_en_wav() {
    let url = premiere_piste(false).await;
    assert!(url.ends_with(".wav"), "même zone, en local : {url}");
}

// ───────────── Index sur une piste longue (encodeur natif) ─────────────

const SR: u32 = 44_100;

/// 35 s de PCM 16 bits stéréo (le banc de #7266 en fait 30).
fn pcm_35_s() -> Vec<u8> {
    let mut x: u32 = 9174;
    (0..SR * 35)
        .flat_map(|n| {
            let t = n as f32 / SR as f32;
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bruit = ((x >> 16) as i16 as f32) / 64.0;
            let g = (8000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() + bruit) as i16;
            let d = (8000.0 * (2.0 * std::f32::consts::PI * 660.0 * t).sin() - bruit) as i16;
            [g.to_le_bytes(), d.to_le_bytes()].concat()
        })
        .collect()
}

/// La SEEKTABLE d'un FLAC : (échantillon, décalage, échantillons) par point,
/// et l'octet de la première trame.
fn table(flac: &[u8]) -> (Vec<(u64, u64, u16)>, usize) {
    let (blocs, debut) = blocs_de_metadonnees(flac).expect("FLAC");
    let corps = blocs
        .iter()
        .find(|(k, _)| *k == 3)
        .map(|(_, c)| c.to_vec())
        .expect("SEEKTABLE");
    let points = corps
        .chunks(18)
        .map(|p| {
            (
                u64::from_be_bytes(p[0..8].try_into().unwrap()),
                u64::from_be_bytes(p[8..16].try_into().unwrap()),
                u16::from_be_bytes([p[16], p[17]]),
            )
        })
        .collect();
    (points, debut)
}

/// Chaque point désigne l'en-tête d'une trame réelle, à partir de laquelle
/// le FLAC se décode jusqu'au bout (le saut d'un `Range` est juste).
fn verifier_les_points(flac: &[u8], points: &[(u64, u64, u16)], debut: usize) {
    assert_eq!(points.len(), 4, "0, 10, 20, 30 s : {points:?}");
    for (k, &(echantillon, decalage, dans_la_trame)) in points.iter().enumerate() {
        let cible = k as u64 * 10 * SR as u64;
        assert!(
            echantillon >= cible && echantillon < cible + 4096,
            "point {k} : {echantillon}"
        );
        assert_eq!(echantillon % 4096, 0, "point {k}");
        assert_eq!(dans_la_trame, 4096, "point {k}");
        let t = &flac[debut + decalage as usize..];
        assert_eq!((t[0], t[1] & 0xFE), (0xFF, 0xF8), "point {k} hors trame");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remux_et_reencodage_posent_la_meme_table_sur_une_piste_longue() {
    let natif = flac_cd(&pcm_35_s()).await;
    let dir = tempfile::tempdir().unwrap();

    // Ré-encodage : le tampon de l'encodeur natif, indexé à l'écriture.
    let reencode = dir.path().join("reencode.flac");
    ecrire_flac_indexe(reencode.to_str().unwrap(), &natif).unwrap();
    let reencode = std::fs::read(&reencode).unwrap();
    let (points_reencode, debut_reencode) = table(&reencode);
    verifier_les_points(&reencode, &points_reencode, debut_reencode);

    // Remux : le même FLAC, en fMP4 comme TIDAL le livre.
    let (info, trames) = trames_flac(&natif);
    let fmp4 = [segment_init(&info), segment_media(1, 0, &trames)].concat();
    let source = dir.path().join("piste.mp4");
    std::fs::write(&source, &fmp4).unwrap();
    let remuxe = dir.path().join("remuxe.flac");
    crate::audio::decode::remux_flac_dash_indexe(
        source.to_str().unwrap(),
        remuxe.to_str().unwrap(),
        true,
    )
    .unwrap();
    let remuxe_octets = std::fs::read(&remuxe).unwrap();
    let (points_remux, debut_remux) = table(&remuxe_octets);
    verifier_les_points(&remuxe_octets, &points_remux, debut_remux);

    assert_eq!(
        points_remux, points_reencode,
        "les deux chemins doivent poser le même index"
    );

    // Et le fichier indexé se décode au même PCM que l'original.
    let brut = dir.path().join("brut.flac");
    std::fs::write(&brut, &natif).unwrap();
    let pcm = |p: &std::path::Path| {
        crate::audio::decode::decode_to_pcm(p.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap()
            .samples_i32
    };
    assert_eq!(pcm(&remuxe), pcm(&brut));
}
