//! #6925 — un épisode de podcast `.m4a` dont l'atome `moov` est en tête se
//! décode au fil de l'eau sur la sortie locale.
//!
//! Laguespa, fil 2415 (rc7, Linux) : les épisodes Radio France ajoutés par
//! leur flux RSS sont des `.m4a` d'une heure (87 Mo), `ftyp` puis `moov` dès
//! le 28e octet. Le journal du rapport :
//!
//! ```text
//! 09:08:42.643 podcast_episode_play zone_id=1 title="Carte blanche à Ibeyi" output_sent=true
//! 09:08:43.011 local_audio_non_wav_stream_detected_decoding
//! 09:08:52.388 demarrage_fige_relance_automatique_5522 zone_id=1 ligne=0 …
//! ```
//!
//! La branche compressée d'avant lisait TOUT le corps avant de décoder : rien
//! ne sonnait pendant le téléchargement, et la garde du démarrage figé
//! relançait la zone à huit secondes. Ces témoins tiennent le premier bloc
//! réel de l'épisode, un M4A dont `moov` est en fin de fichier (qui garde la
//! branche d'avant), et un M4A servi lentement qui doit sonner bien avant la
//! fin de son transfert.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::decodage_en_continu::{
    EnteteDecodee, extension_decodable_en_continu, lire_l_entete_decodee,
};
use super::decodage_en_continu_5439::{CADENCE, CANAUX, ServeurLent};
use super::*;

const DUREE_S: u32 = 8;
/// Le transfert complet imposé par [`ServeurLent`].
const TRANSFERT_COMPLET_MS: u64 = 4_000;
/// 500 ms de PCM décodé (32 bits) : le pré-remplissage du chemin PCM.
const PRE_REMPLISSAGE_OCTETS: usize = (CADENCE as usize / 2) * CANAUX as usize * 4;
/// Même borne que #5439 : sous 40 % du transfert complet.
const PREMIER_SON_MAX_MS: u64 = 1_500;

/// Les 48 premiers octets de l'épisode Banzzaï du 23/10/2026
/// (`media.radiofrance-podcast.net/podcast09/26875-23.10.2026-…m4a`, servi
/// `audio/mp4`, annoncé `audio/mpeg` dans le flux) : `ftyp` de 28 octets, puis
/// `moov` de 691 299 octets.
const ENTETE_RADIO_FRANCE: [u8; 48] = [
    0x00, 0x00, 0x00, 0x1c, b'f', b't', b'y', b'p', b'M', b'4', b'A', b' ', 0x00, 0x00, 0x02,
    0x00, b'M', b'4', b'A', b' ', b'i', b's', b'o', b'm', b'i', b's', b'o', b'2', 0x00, 0x0a,
    0x8c, 0x63, b'm', b'o', b'o', b'v', 0x00, 0x00, 0x00, 0x6c, b'm', b'v', b'h', b'd', 0x00,
    0x00, 0x00, 0x00,
];

/// Un ALAC 48 kHz / 16 bits stéréo de [`DUREE_S`] s, tel que le multiplexeur
/// du dépôt l'écrit : `ftyp`, `mdat`, puis `moov` en fin de fichier.
fn m4a_moov_en_fin() -> Vec<u8> {
    let trames = (CADENCE * DUREE_S) as usize;
    let mut echantillons = Vec::with_capacity(trames * CANAUX as usize);
    let mut graine: u32 = 0x6925;
    for i in 0..trames {
        let t = i as f64 / CADENCE as f64;
        for voie in 0..CANAUX {
            graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bruit = ((graine >> 20) as f64 / 4096.0 - 0.5) * 0.02;
            let f = if voie == 0 { 440.0 } else { 660.0 };
            let v = (2.0 * std::f64::consts::PI * f * t).sin() * 0.5 + bruit;
            echantillons.push((v * i16::MAX as f64) as i32);
        }
    }
    crate::audio::alac_encoder::encode_alac_m4a(&echantillons, 16, CANAUX, CADENCE)
        .expect("encodage ALAC")
}

#[test]
fn podcast_6925_le_premier_bloc_reel_de_radio_france_se_decode_en_continu() {
    assert_eq!(
        extension_decodable_en_continu(&ENTETE_RADIO_FRANCE),
        Some("m4a"),
        "un M4A dont `moov` suit `ftyp` doit se décoder pendant le téléchargement (#6925)"
    );
}

#[test]
fn podcast_6925_un_moov_en_fin_de_fichier_garde_la_branche_d_avant() {
    let fin = m4a_moov_en_fin();
    assert_eq!(&fin[4..8], b"ftyp");
    assert_eq!(
        extension_decodable_en_continu(&fin[..4096.min(fin.len())]),
        None,
        "`mdat` avant `moov` : illisible sans rembobiner"
    );
    let tete = crate::audio::faststart::faststart_m4a(&fin).expect("réécriture moov en tête");
    assert_eq!(
        extension_decodable_en_continu(&tete[..4096.min(tete.len())]),
        Some("m4a")
    );
    // Un bloc trop court pour voir `moov` ne promet rien.
    assert_eq!(extension_decodable_en_continu(&ENTETE_RADIO_FRANCE[..30]), None);
}

#[test]
fn podcast_6925_un_m4a_servi_lentement_sonne_avant_la_fin_du_transfert() {
    let fin = m4a_moov_en_fin();
    let tete = Arc::new(crate::audio::faststart::faststart_m4a(&fin).expect("moov en tête"));
    let serveur = ServeurLent::servir(tete.clone());
    let arret = Arc::new(AtomicBool::new(false));
    let debut = Instant::now();

    // Ce que `play_url` fait du premier bloc.
    let mut lecteur =
        LecteurHttpAnnulable::ouvrir(&serveur.url, arret.clone()).expect("ouverture HTTP");
    let mut entete = vec![0u8; 4096];
    let lus = loop {
        match lecteur.read(&mut entete) {
            Ok(n) => break n,
            Err(ref e) if header_read_should_retry(e.kind()) => continue,
            Err(e) => panic!("lecture du premier bloc : {e}"),
        }
    };
    entete.truncate(lus);
    assert!(parse_wav_header(&entete).is_none());
    let extension = extension_decodable_en_continu(&entete)
        .expect("le M4A à moov en tête prend le décodage continu, pas la branche qui charge tout");
    let mut lecteur = lecteur.decoder_en_continu(entete, extension);
    let (mut octets, format) = match lire_l_entete_decodee(&mut lecteur, &arret) {
        EnteteDecodee::Pret { octets, format } => (octets, format),
        EnteteDecodee::Interrompu => panic!("en-tête interrompu"),
        EnteteDecodee::Echec => panic!("décodage : {:?}", lecteur.echec_du_decodage()),
    };
    assert_eq!((format.0, format.1, format.2), (CANAUX, CADENCE, 32));

    let mut bloc = vec![0u8; 16_384];
    while octets.len() < format.3 + PRE_REMPLISSAGE_OCTETS {
        let n = lecteur.read(&mut bloc).expect("flux décodé");
        assert!(n > 0, "le flux décodé s'est tari avant 500 ms d'audio");
        octets.extend_from_slice(&bloc[..n]);
    }
    let premier_son_ms = debut.elapsed().as_millis() as u64;
    lecteur
        .read_to_end(&mut octets)
        .expect("lecture du flux décodé");
    let fin_ms = debut.elapsed().as_millis() as u64;
    eprintln!("6925 : 500 ms d'audio à {premier_son_ms} ms, fin du flux à {fin_ms} ms");

    assert!(
        premier_son_ms < PREMIER_SON_MAX_MS,
        "500 ms d'audio prêts seulement après {premier_son_ms} ms sur un transfert de \
         {TRANSFERT_COMPLET_MS} ms : l'épisode attend encore la fin du téléchargement (#6925)"
    );
    assert!(
        fin_ms >= TRANSFERT_COMPLET_MS * 9 / 10,
        "le serveur a bien servi lentement ({fin_ms} ms)"
    );
    let trames = ((octets.len() - format.3) / (CANAUX as usize * 4)) as u64;
    let attendu = (CADENCE * DUREE_S) as u64;
    assert!(
        trames.abs_diff(attendu) <= 4_096,
        "{trames} trames décodées pour {attendu} attendues"
    );
}
