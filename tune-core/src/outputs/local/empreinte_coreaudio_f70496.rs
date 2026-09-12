//! REF-8 (#2219) — **la preuve d'identité du rendu du bras CoreAudio**, sur le
//! chemin décoder → étage → puits.
//!
//! Le bras CoreAudio exclusif (`bras_coreaudio.rs`) avait sa propre boucle :
//! `process_pcm_chunk` sur l'amorce, puis lecture par blocs de 65 536 octets,
//! `process_pcm_chunk`, écriture directe dans l'anneau. REF-8 la fait
//! disparaître : le bras monte un [`EtageDeConversion`] au format identité et
//! appelle la [`BoucleProducteur`] commune, celle du chemin partagé.
//!
//! Ces témoins branchent le puits de capture de `tune-output-api`
//! ([`CaptureOutput`]) à la place de l'anneau, sur TROIS signaux, et comparent
//! l'empreinte FNV-1a à une constante **relevée sur la route directe d'AVANT**
//! (`986d2f0f`, `outputs/` identique sur la tête `49ecf1fe`), par un fichier
//! de mesure temporaire rejouant la boucle propre du bras mot pour mot. Les
//! constantes sont des RELEVÉS, pas des valeurs à ajuster : si l'une ne tombe
//! plus, la réorganisation a changé ce qui part à l'AudioUnit.
//!
//! Ce qui est mesuré : la suite décoder → étage → boucle commune → puits, sur
//! Shrek, sans `cfg(macos)`. Ce qui ne l'est pas, et ne peut pas l'être ici :
//! le HAL, le rappel `Interleaved<f32>` et la conversion f32 → format physique
//! que l'AudioUnit fait hors du dépôt (D1 : mot f32 conservé — l'écoute de
//! Bertrand est la seule porte de ce dernier maillon).
//!
//! Chaque signal est coupé comme le bras le voit : une **amorce** (ce qui
//! suit l'en-tête WAV dans les 4 096 premiers octets, 4 052 octets — pour le
//! 24 bits, une coupure au milieu d'une trame) puis le **reste** servi par un
//! `Read`, comme la réponse HTTP.

use std::sync::atomic::{AtomicBool, AtomicU64};

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::{
    BoucleProducteur, CompteursDePiste, EtageDeConversion, FinDeBoucle, PousseeVersLePuits,
    RoleDeLaBoucle,
};
use crate::outputs::traits::{CaptureOutput, FormatOuvert};

/// La coupure de l'amorce : 4 096 octets lus pour l'en-tête, moins un
/// en-tête WAV canonique de 44 octets.
const AMORCE: usize = 4096 - 44;

/// Une rampe de mots 16 bits déterministe, pleine échelle et changeant de
/// signe — la même que `empreinte_du_puits_r1`, pour que le relevé identité
/// soit comparable au sien.
fn pcm16(trames: usize, channels: u16) -> Vec<u8> {
    let mut octets = Vec::with_capacity(trames * channels as usize * 2);
    let mut graine: i32 = 1;
    for _ in 0..trames * channels as usize {
        graine = (graine.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7fff_ffff;
        let mot = ((graine >> 8) as i16).wrapping_sub(i16::MAX / 2);
        octets.extend_from_slice(&mot.to_le_bytes());
    }
    octets
}

/// Une rampe de mots 24 bits, petit-boutiste, trois octets par mot : le cas
/// nominal d'une sortie CoreAudio exclusive (24/96), et celui où la sonde
/// DoP de 32 trames doit conclure « PCM » avant de rendre un mot.
fn pcm24(trames: usize, channels: u16) -> Vec<u8> {
    let mut octets = Vec::with_capacity(trames * channels as usize * 3);
    let mut graine: i32 = 7;
    for _ in 0..trames * channels as usize {
        graine = (graine.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7fff_ffff;
        let mot = (graine >> 7).wrapping_sub(1 << 22);
        octets.extend_from_slice(&mot.to_le_bytes()[..3]);
    }
    octets
}

/// La fixture DoP versionnée du dépôt : 64 trames stéréo 24 bits, la sortie
/// réelle de l'encodeur (`versioned_dop_fixture_is_the_real_encoder_output_
/// byte_for_byte`). Sur ce bras elle n'est ni refusée ni verrouillée : elle
/// traverse en f32, volume figé à l'unité.
fn fixture_dop() -> Vec<u8> {
    include_str!("../../../tests/fixtures/dop_stereo_24le_64frames.hex")
        .split_ascii_whitespace()
        .map(|octet| u8::from_str_radix(octet, 16).expect("fixture DoP hex valide"))
        .collect()
}

/// Le chemin du bras, tel qu'il est maintenant : amorce par `etage.pousser`,
/// puis la boucle commune sur un `Read`, au format identité, sans refus DoP.
fn route_du_bras(
    amorce: Vec<u8>,
    reste: &[u8],
    sample_rate: u32,
    channels: u16,
    bit_depth: u16,
) -> CaptureOutput {
    let dsp = DspAuRepos::neuf();
    let mut e: EtageDeConversion<'_> = etage(
        &dsp,
        amorce,
        sample_rate,
        channels,
        bit_depth,
        sample_rate,
        channels,
    );
    assert!(
        !e.needs_channel_adapt() && !e.needs_resample,
        "le bras CoreAudio ouvre au format source : l'étage doit être l'identité"
    );
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(sample_rate, channels));
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;

    let mut trames = 0u64;
    match e.pousser(&mut puits, &mut refus, &mut |_| {}) {
        PousseeVersLePuits::Poussee { trames_source }
        | PousseeVersLePuits::PuitsMort { trames_source } => trames += trames_source,
        PousseeVersLePuits::RienAPousser => {}
        PousseeVersLePuits::PorteurDopRefuse => panic!("ce bras ne refuse jamais le DoP"),
    }

    let (_stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let force_silent = AtomicBool::new(false);
    let device_gone = AtomicBool::new(false);
    let position_ms = AtomicU64::new(0);
    let open_failure = std::sync::Mutex::new(None);
    let producteur = BoucleProducteur {
        role: RoleDeLaBoucle::PisteInitiale,
        device_name: "témoin CoreAudio",
        cle_de_flux: None,
        stop_rx: &stop_rx,
        force_silent: &force_silent,
        device_gone: &device_gone,
        position_ms: &position_ms,
        open_failure: &open_failure,
        debut_du_flux: std::time::Instant::now(),
    };
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: trames,
        seek_offset: 0,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    let mut lecture = std::io::Cursor::new(reste);
    let mut tampon = vec![0u8; 65536];
    let fin = producteur.tourner(
        &mut lecture,
        &mut tampon,
        &mut e,
        &mut puits,
        &mut refus,
        &mut compteurs,
        &mut |_| true,
    );
    assert!(
        matches!(fin, FinDeBoucle::FinDeFlux),
        "la boucle commune doit sortir sur la fin du flux"
    );
    assert!(
        open_failure.lock().unwrap().is_none(),
        "aucun échec ne doit être rapporté sur un puits vivant"
    );
    puits
}

// ───────────────────────────────────────────────────────────────────────────
// Les relevés. Mesurés sur la route DIRECTE du bras (`process_pcm_chunk` →
// anneau, boucle propre) sur `49ecf1fe` (= `986d2f0f` pour `outputs/`), puis
// rejoués ici à travers l'étage et la boucle commune.
// ───────────────────────────────────────────────────────────────────────────

/// Identité 16 bits stéréo 44,1 kHz — le chemin de la majorité des lectures.
/// Même signal que le relevé identité de R1 : le bras et le chemin partagé
/// rendent les mêmes mots au même puits.
#[test]
fn le_bras_coreaudio_rend_les_memes_mots_en_16_bits_stereo() {
    let signal = pcm16(2048, 2);
    let (amorce, reste) = signal.split_at(AMORCE);
    let puits = route_du_bras(amorce.to_vec(), reste, 44_100, 2, 16);

    assert_eq!(puits.mots(), 4096, "2048 trames stéréo font 4096 mots");
    assert_eq!(
        puits.empreinte(),
        EMPREINTE_COREAUDIO_IDENTITE_16_BITS_STEREO,
        "le puits ne reçoit plus les mêmes mots que la route directe du bras \
         (16 bits stéréo 44,1 kHz)"
    );
}

/// 24 bits stéréo 96 kHz — le cas nominal d'une sortie exclusive, avec une
/// amorce coupée au milieu d'une trame (4 052 n'est pas un multiple de 6) :
/// le reliquat non aligné doit être reporté, pas jeté, sans quoi tous les
/// mots suivants sont lus au mauvais décalage d'octet (bruit blanc).
#[test]
fn le_bras_coreaudio_rend_les_memes_mots_en_24_bits() {
    let signal = pcm24(4096, 2);
    let (amorce, reste) = signal.split_at(AMORCE);
    let puits = route_du_bras(amorce.to_vec(), reste, 96_000, 2, 24);

    assert_eq!(puits.mots(), 8192, "4096 trames stéréo font 8192 mots");
    assert_eq!(
        puits.empreinte(),
        EMPREINTE_COREAUDIO_24_BITS_STEREO_96K,
        "le puits ne reçoit plus les mêmes mots que la route directe du bras \
         (24 bits stéréo 96 kHz, amorce non alignée)"
    );
}

/// DoP — le porteur traverse ce bras EN f32, ni refusé ni verrouillé (D1).
/// La fixture tient dans l'amorce ; la sonde de 32 trames conclut « DoP »,
/// `sync_volume_to_dop` fige le volume, et les 128 mots partent tels quels.
#[test]
fn le_bras_coreaudio_laisse_traverser_le_dop_a_l_identique() {
    let fixture = fixture_dop();
    assert_eq!(fixture.len(), 64 * 2 * 3, "64 trames stéréo 24 bits");
    let puits = route_du_bras(fixture, &[], 176_400, 2, 24);

    assert_eq!(puits.mots(), 128, "64 trames stéréo font 128 mots");
    assert_eq!(
        puits.empreinte(),
        EMPREINTE_COREAUDIO_DOP_FIXTURE,
        "le porteur DoP ne traverse plus le bras à l'identique : un mot changé \
         et le DAC ne voit plus le marqueur 0x05/0xFA"
    );
}

// Les relevés eux-mêmes. Voir l'en-tête du fichier : ce sont des MESURES
// prises sur la route directe d'avant, pas des valeurs à ajuster.
const EMPREINTE_COREAUDIO_IDENTITE_16_BITS_STEREO: u64 = 0x1433_8456_2279_0c63;
const EMPREINTE_COREAUDIO_24_BITS_STEREO_96K: u64 = 0x8682_1b97_c7ef_459b;
const EMPREINTE_COREAUDIO_DOP_FIXTURE: u64 = 0x94d6_4036_7eeb_48a5;
