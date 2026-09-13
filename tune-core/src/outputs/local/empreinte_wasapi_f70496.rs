//! REF-8 (#2219) — **la preuve d'identité du rendu du bras WASAPI**, avant et
//! après son passage au puits natif.
//!
//! Le bras WASAPI exclusif décode des octets source en mots `i32` alignés à
//! gauche (`prepare_windows_native_pcm`), les pousse dans un anneau entier,
//! et le fil de rendu les resérialise en octets par `pop_pcm_bytes`. REF-8
//! intercale un étage natif ([`super::etage_natif::EtageNatif`]) et un puits
//! ([`super::etage_natif::PuitsAnneauNatif`]) entre les deux : ces témoins
//! vérifient que **pas un octet** n'a changé de route.
//!
//! Même méthode que `empreinte_du_puits_r1` : un puits qui hache tout ce qu'il
//! reçoit ([`CaptureOutputNatif`]), et des constantes **RELEVÉES sur la
//! version d'AVANT** — la route directe `prepare_windows_native_pcm` →
//! mots `i32` → octets, jouée sur les mêmes signaux au commit `49ecf1fe` de
//! `batch/bugs-12` (dont `outputs/` est identique à `986d2f0f`). Si l'une
//! d'elles ne tombe plus, la réorganisation a changé le rendu, et c'est elle
//! qui a tort.
//!
//! Trois signaux : 16 bits identité, 24 bits identité, et la fixture DoP
//! versionnée (`tests/fixtures/dop_stereo_24le_64frames.hex`, la sortie réelle
//! de l'encodeur, gardée octet pour octet par
//! `versioned_dop_fixture_is_the_real_encoder_output_byte_for_byte`).
//!
//! Ce module ne compile que sous `test` (Shrek) : il n'a pas besoin de Windows,
//! parce que tout ce qu'il mesure vit sous `cfg(any(target_os = "windows",
//! test))`. C'est la raison d'être de ce `cfg` : être jugé AVANT la CI Windows.

use std::sync::atomic::{AtomicBool, AtomicU32};

use super::etage_natif::{EcritureNative, EtageNatif, PuitsAnneauNatif, mots_natifs_du_bloc};
use super::{NativePcmRing, native_i32_to_pcm_bytes, prepare_windows_native_pcm};
use crate::outputs::traits::{
    AudioSpec, CaptureOutputNatif, FormatOuvert, ProfondeurPcm, PuitsNatif,
};

/// Relevés AVANT, sur `49ecf1fe` : la route directe
/// `prepare_windows_native_pcm` → `i32::to_le_bytes` → `CaptureOutputNatif`
/// ouvert en `Entier32`, volume à l'unité, DSP au repos.
const EMPREINTE_16_BITS_IDENTITE: u64 = 0x481b_b7a5_7cc8_3693;
const EMPREINTE_24_BITS_IDENTITE: u64 = 0x0aa9_903d_6441_146d;
const EMPREINTE_DOP_FIXTURE: u64 = 0x12e6_821d_877b_2aa5;

/// Le DSP au repos et le volume à l'unité : l'état d'une zone qui ne fait
/// que lire, celui où le producteur WASAPI prend la branche « octets source
/// conservés » (`bit_perfect`).
struct DspAuRepos {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    volume: AtomicU32,
}

impl DspAuRepos {
    fn neuf() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            volume: AtomicU32::new(1000),
        }
    }

    fn etage(&self, spec: AudioSpec) -> EtageNatif<'_> {
        EtageNatif::monter(
            spec,
            FormatOuvert::new(spec.cadence(), spec.canaux()),
            &self.volume,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
    }
}

/// Un signal stéréo déterministe de `frames` trames, `octets_par_mot` octets
/// par mot, petit-boutien : un générateur congruentiel dont on prend les
/// octets bas — les deux signes, tous les bits bas, aucun marqueur DoP.
fn signal(frames: usize, octets_par_mot: usize) -> Vec<u8> {
    let mut etat: u32 = 0x2545_f491;
    (0..frames * 2 * octets_par_mot)
        .map(|_| {
            etat = etat.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (etat >> 24) as u8
        })
        .collect()
}

fn fixture_dop() -> Vec<u8> {
    include_str!("../../../tests/fixtures/dop_stereo_24le_64frames.hex")
        .split_ascii_whitespace()
        .map(|octet| u8::from_str_radix(octet, 16).expect("fixture DoP hex valide"))
        .collect()
}

fn spec_source(profondeur: ProfondeurPcm) -> AudioSpec {
    AudioSpec::nouvelle(44_100, profondeur, 2).expect("stéréo")
}

/// Le format des BLOCS que l'étage natif livre : des mots `i32` alignés à
/// gauche, quatre octets petit-boutiens chacun, quelle que soit la profondeur
/// de la source. C'est `ProfondeurPcm::Entier32` — voir `etage_natif.rs`.
fn spec_du_puits(source: AudioSpec) -> AudioSpec {
    AudioSpec::nouvelle(source.cadence(), ProfondeurPcm::Entier32, source.canaux()).expect("stéréo")
}

/// La route d'AVANT, telle que `bras_wasapi.rs` l'appelait à `49ecf1fe` :
/// `prepare_windows_native_pcm` puis les mots sérialisés en octets.
fn route_directe_d_avant(dsp: &DspAuRepos, octets: &[u8], bit_depth: u16) -> u64 {
    let prepared = prepare_windows_native_pcm(
        octets,
        bit_depth,
        2,
        true,
        false,
        1000,
        &dsp.eq,
        &dsp.convolver,
        &dsp.crossfeed,
        &dsp.pure_bypass,
        &dsp.mono_downmix,
    )
    .expect("fenêtre PCM complète");
    let mots: Vec<u8> = prepared
        .samples
        .iter()
        .flat_map(|mot| mot.to_le_bytes())
        .collect();
    let spec = spec_du_puits(spec_source(
        ProfondeurPcm::depuis_bits_declares(bit_depth).expect("profondeur du jeu fermé"),
    ));
    let mut capture = CaptureOutputNatif::ouvrir(spec);
    assert!(capture.ecrire(spec.bloc(&mots)));
    capture.empreinte()
}

/// La route d'APRÈS : l'étage natif écrit dans le puits de capture.
fn route_de_l_etage(dsp: &DspAuRepos, octets: &[u8], profondeur: ProfondeurPcm) -> u64 {
    let source = spec_source(profondeur);
    let mut etage = dsp.etage(source);
    let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits(source));
    match etage.decoder_et_pousser(octets, &mut capture) {
        EcritureNative::Poussee {
            trames_source,
            bit_perfect,
            ..
        } => {
            assert_eq!(
                trames_source as usize,
                octets.len() / source.octets_par_trame()
            );
            assert!(
                bit_perfect,
                "volume à l'unité et DSP au repos : la branche est brute"
            );
        }
        autre => panic!("l'étage natif n'a rien poussé : {autre:?}"),
    }
    assert_eq!(capture.blocs_refuses(), 0, "{:?}", capture.dernier_refus());
    assert_eq!(capture.reste_en_attente(), &[] as &[u8]);
    capture.empreinte()
}

/// La route jusqu'au DAC : l'étage écrit dans le puits d'anneau, le fil de
/// rendu resérialise par `pop_pcm_bytes`. Rend ce que le DAC recevrait.
fn route_jusqu_au_dac(dsp: &DspAuRepos, octets: &[u8], profondeur: ProfondeurPcm) -> Vec<u8> {
    let source = spec_source(profondeur);
    let mut etage = dsp.etage(source);
    let anneau = std::sync::Arc::new(NativePcmRing::new(octets.len()));
    let (_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let force_silent = AtomicBool::new(false);
    let mut puits = PuitsAnneauNatif::sur(
        anneau.clone(),
        spec_du_puits(source),
        &stop_rx,
        &paused,
        &force_silent,
    );
    assert!(matches!(
        etage.decoder_et_pousser(octets, &mut puits),
        EcritureNative::Poussee { .. }
    ));
    let mut au_dac = vec![0xAAu8; octets.len()];
    let ecrits = anneau.pop_pcm_bytes(&mut au_dac, profondeur.bits_declares());
    assert_eq!(ecrits, octets.len());
    au_dac
}

#[test]
fn le_puits_natif_recoit_les_memes_mots_pour_le_16_bits_identite() {
    let dsp = DspAuRepos::neuf();
    let octets = signal(256, 2);
    assert_eq!(
        route_directe_d_avant(&dsp, &octets, 16),
        EMPREINTE_16_BITS_IDENTITE,
        "la route directe d'avant ne rend plus le relevé : `prepare_windows_native_pcm` a changé"
    );
    assert_eq!(
        route_de_l_etage(&dsp, &octets, ProfondeurPcm::Entier16),
        EMPREINTE_16_BITS_IDENTITE,
        "l'étage natif ne rend pas les mots que le bras rendait à 49ecf1fe (16 bits)"
    );
    assert_eq!(
        route_jusqu_au_dac(&dsp, &octets, ProfondeurPcm::Entier16),
        octets,
        "ce que le fil de rendu WASAPI resérialise n'est plus la source (16 bits)"
    );
}

#[test]
fn le_puits_natif_recoit_les_memes_mots_pour_le_24_bits_identite() {
    let dsp = DspAuRepos::neuf();
    let octets = signal(256, 3);
    assert_eq!(
        route_directe_d_avant(&dsp, &octets, 24),
        EMPREINTE_24_BITS_IDENTITE,
        "la route directe d'avant ne rend plus le relevé : `prepare_windows_native_pcm` a changé"
    );
    assert_eq!(
        route_de_l_etage(&dsp, &octets, ProfondeurPcm::Entier24),
        EMPREINTE_24_BITS_IDENTITE,
        "l'étage natif ne rend pas les mots que le bras rendait à 49ecf1fe (24 bits)"
    );
    assert_eq!(
        route_jusqu_au_dac(&dsp, &octets, ProfondeurPcm::Entier24),
        octets,
        "ce que le fil de rendu WASAPI resérialise n'est plus la source (24 bits)"
    );
}

#[test]
fn le_puits_natif_recoit_les_memes_mots_pour_le_porteur_dop() {
    let dsp = DspAuRepos::neuf();
    let octets = fixture_dop();
    assert_eq!(
        route_directe_d_avant(&dsp, &octets, 24),
        EMPREINTE_DOP_FIXTURE,
        "la route directe d'avant ne rend plus le relevé : `prepare_windows_native_pcm` a changé"
    );
    assert_eq!(
        route_de_l_etage(&dsp, &octets, ProfondeurPcm::Entier24),
        EMPREINTE_DOP_FIXTURE,
        "l'étage natif ne rend pas les mots que le bras rendait à 49ecf1fe (DoP)"
    );
    assert_eq!(
        route_jusqu_au_dac(&dsp, &octets, ProfondeurPcm::Entier24),
        octets,
        "un marqueur DoP a été touché entre l'étage et le fil de rendu"
    );
    // La décision DoP reste dans l'étage : il la dit, il ne la refuse pas.
    let mut etage = dsp.etage(spec_source(ProfondeurPcm::Entier24));
    let mut capture =
        CaptureOutputNatif::ouvrir(spec_du_puits(spec_source(ProfondeurPcm::Entier24)));
    assert!(matches!(
        etage.decoder_et_pousser(&octets, &mut capture),
        EcritureNative::Poussee {
            dop: true,
            bit_perfect: true,
            ..
        }
    ));
    assert!(etage.dop_verrouille());
}

/// Les trois relevés sont distincts : une empreinte qui ne bougerait sur aucun
/// signal ne prouverait rien.
#[test]
fn les_trois_releves_sont_distincts() {
    assert_ne!(EMPREINTE_16_BITS_IDENTITE, EMPREINTE_24_BITS_IDENTITE);
    assert_ne!(EMPREINTE_24_BITS_IDENTITE, EMPREINTE_DOP_FIXTURE);
    assert_ne!(EMPREINTE_16_BITS_IDENTITE, EMPREINTE_DOP_FIXTURE);
}

/// Le mot du bloc est un `i32` petit-boutien : `mots_natifs_du_bloc` est
/// l'inverse exact de `i32::to_le_bytes`, et ce que le puits d'anneau pousse
/// est ce que `native_i32_to_pcm_bytes` resérialiserait à l'identique.
#[test]
fn le_mot_du_bloc_est_l_i32_natif_petit_boutien() {
    let source = signal(64, 3);
    let mots = super::pcm_bytes_to_native_i32(&source, 24);
    let octets: Vec<u8> = mots.iter().flat_map(|mot| mot.to_le_bytes()).collect();
    let spec = spec_du_puits(spec_source(ProfondeurPcm::Entier24));
    assert_eq!(mots_natifs_du_bloc(spec.bloc(&octets)), mots);
    let mut retour = vec![0u8; source.len()];
    native_i32_to_pcm_bytes(&mots, 24, &mut retour);
    assert_eq!(retour, source);
}
