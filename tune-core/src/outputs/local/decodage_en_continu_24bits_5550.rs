//! #5550 — un FLAC 24 bits servi par un serveur multimédia reste bit-perfect
//! jusqu'au puits de la sortie locale.
//!
//! Les épreuves de #5439 (`decodage_en_continu_5439.rs`) comparent le flux
//! décodé au fichier local, mais sur une source 16 bits seulement. Celle-ci
//! tient les trois exigences du bit-perfect sur une source 24 bits pleine
//! échelle, bits de poids faible compris :
//!
//! 1. le flux décodé est identique, octet pour octet, au décodage du même
//!    fichier posé sur disque ;
//! 2. la profondeur est conservée : chaque mot 32 bits vaut la valeur 24 bits
//!    de la source décalée de 8, sans arrondi ni tramage ;
//! 3. à la cadence de la source et DSP au repos, le puits reçoit les 24 bits
//!    exacts : aucun rééchantillonneur, aucune altération.

use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
use std::time::Instant;

use super::decodage_en_continu_5439::{
    CADENCE, CANAUX, ServeurLent, ouvrir_comme_play_url, reference_fichier_local,
};
use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::*;
use crate::outputs::traits::{CaptureOutput, FormatOuvert};

const DUREE_S: u32 = 2;
/// 2^23 : un mot 24 bits signé ramené en flottant dans [-1, 1).
const ECHELLE_24_BITS: f64 = 8_388_608.0;

/// Un FLAC 48 kHz / 24 bits stéréo de [`DUREE_S`] s, fait d'un bruit
/// déterministe qui occupe les 24 bits : une perte de profondeur, même d'un
/// seul bit de poids faible, se voit sur presque chaque mot.
fn flac_48k_24_bits() -> (Vec<u8>, Vec<i32>) {
    let trames = (CADENCE * DUREE_S) as usize;
    let mut pcm = Vec::with_capacity(trames * CANAUX as usize * 3);
    let mut valeurs = Vec::with_capacity(trames * CANAUX as usize);
    let mut graine: u32 = 0x5550;
    for _ in 0..trames {
        for _ in 0..CANAUX {
            graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v: i32 = (graine as i32) >> 8;
            valeurs.push(v);
            pcm.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    let mut encodeur = crate::audio::encoder::AudioEncoder::new("flac", CADENCE, 24, CANAUX as u32);
    encodeur.start_sync().expect("début FLAC");
    encodeur.write_sync(&pcm).expect("écriture FLAC");
    (encodeur.finish_sync().expect("fin FLAC"), valeurs)
}

#[test]
fn chemin_compresse_5550_un_flac_24_bits_reste_bit_perfect_jusqu_au_puits() {
    let (flac, valeurs) = flac_48k_24_bits();
    let flac = Arc::new(flac);
    let reference = reference_fichier_local(&flac);

    let serveur = ServeurLent::servir(flac.clone());
    let arret = Arc::new(AtomicBool::new(false));
    let (mut lecteur, mut octets, format) = ouvrir_comme_play_url(&serveur.url, &arret);
    assert_eq!(
        (format.0, format.1, format.2),
        (CANAUX, CADENCE, 32),
        "le flux décodé garde la cadence et les canaux de la source, en mots de 32 bits"
    );
    lecteur
        .read_to_end(&mut octets)
        .expect("lecture du flux décodé");

    // 1. Octet pour octet, en-tête compris, comme un fichier local.
    assert!(
        octets == reference,
        "24 bits : le flux décodé du serveur multimédia diffère du décodage du même fichier local"
    );

    // 2. La profondeur : les 24 bits de la source, décalés dans le mot 32 bits.
    let donnees = &octets[format.3..];
    assert_eq!(donnees.len() / 4, valeurs.len(), "toute la piste");
    for (i, mot) in donnees.as_chunks::<4>().0.iter().enumerate() {
        let w = i32::from_le_bytes(*mot);
        assert_eq!(
            w,
            valeurs[i] << 8,
            "mot {i} : la valeur 24 bits de la source n'arrive pas intacte"
        );
    }

    // 3. Le puits, à la cadence de la source et DSP au repos.
    let dsp = DspAuRepos::neuf();
    let mut conversion = etage(&dsp, Vec::new(), CADENCE, CANAUX, 32, CADENCE, CANAUX);
    assert!(
        conversion.resampler.is_none(),
        "même cadence : aucun rééchantillonneur"
    );
    let mut puits = CaptureOutput::avec_retenue(FormatOuvert::new(CADENCE, CANAUX), valeurs.len());
    let disparu = AtomicBool::new(false);
    let silence_force = AtomicBool::new(false);
    let position = AtomicU64::new(0);
    let erreur = std::sync::Mutex::new(None);
    let (_tx, rx) = mpsc::channel();
    let producteur = BoucleProducteur {
        role: RoleDeLaBoucle::PisteInitiale,
        backend: "capture",
        device_name: "5550",
        cle_de_flux: None,
        stop_rx: &rx,
        force_silent: &silence_force,
        device_gone: &disparu,
        position_ms: &position,
        open_failure: &erreur,
        debut_du_flux: Instant::now(),
        duree_de_la_piste_ms: &DUREE_DE_PISTE_INCONNUE,
        cretes_de_sortie: None,
    };
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset: 0,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    let mut amont = Cursor::new(donnees.to_vec());
    let mut tampon = vec![0u8; 16_384];
    producteur.tourner(
        &mut amont,
        &mut tampon,
        &mut conversion,
        &mut puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |_| true,
    );

    assert!(
        puits.retenue_complete(),
        "tout ce qui a été livré a été retenu"
    );
    let mots = puits.mots_livres().expect("puits à retenue");
    assert_eq!(mots.len(), valeurs.len(), "le puits reçoit toute la piste");
    let ecarts = mots
        .iter()
        .zip(&valeurs)
        .filter(|&(&f, &v)| f as f64 * ECHELLE_24_BITS != v as f64)
        .count();
    assert_eq!(
        ecarts,
        0,
        "{ecarts} mots sur {} ne rendent pas au puits la valeur 24 bits exacte de la source",
        mots.len()
    );
}
