//! REF-8 (#2219) — **les empreintes des deux routes du bras ASIO, relevées
//! AVANT son passage au trait `BackendLocal`.**
//!
//! Le bras ASIO (`local/bras_asio.rs`) n'est compilé que par l'étape « ASIO »
//! de la CI Windows : aucun test ne l'exécute. Ce qu'on PEUT juger sur Shrek,
//! c'est ce qui atteint ses deux anneaux — et c'est exactement ce que REF-8
//! déplace :
//!
//! * **route native** (`Native*`) : avant, `feed_windows_native_exclusive_leftover`
//!   poussait `prepare_windows_native_pcm(…).samples` (des `i32` alignés à
//!   gauche) dans `NativePcmRing` ; après, `EtageNatif::decoder_et_pousser`
//!   pousse ces mêmes mots, sérialisés petit-boutistes, dans un `PuitsNatif`
//!   posé sur le même anneau. Les relevés sont pris sur la chaîne d'AVANT
//!   (les aides de `local.rs`, intouchées) et comparés à ce que l'étage rend ;
//! * **route traitée** (`Processed*`) : avant, `prepare_windows_exclusive_pcm`
//!   rendait des `f32` que `feed_ring_abortable` poussait dans `RingBuf` ;
//!   après, c'est l'étage de R1 (`EtageDeConversion::pousser`) avec une
//!   fermeture qui refuse TOUT porteur DoP. Même relevé, même comparaison.
//!
//! Les constantes sont des RELEVÉS pris sur `49ecf1fe` (tête de
//! `batch/bugs-12`, `986d2f0f` compris), pas des valeurs attendues qu'on
//! ajusterait : si l'une ne tombe plus, un octet a changé de route vers le
//! DAC, et c'est la réorganisation qui a tort.
//!
//! Le puits de capture est celui de #2218 (`CaptureOutputNatif`,
//! `CaptureOutput`) : FNV-1a 64 bits sur les octets livrés, dans l'ordre.
//!
//! Ce que ce témoin ne couvre PAS : le rappel de rendu (`pop_mapped`, volume
//! en `f64` sur la route traitée), le fil pompe, le vidage — rien de ce qui
//! exige un pilote. Le rappel est gardé par
//! `native_windows_ring_is_exact_at_asio_i16_and_i24_callback_boundaries`
//! (`tests.rs`), inchangé.

use super::etage_natif::{EcritureNative, EtageNatif, spec_du_puits_natif};
use super::*;
use super::{EtageDeConversion, LocalPcmKind, LocalPcmProcessor, PousseeVersLePuits};
use crate::outputs::traits::{
    AudioSpec, CaptureOutput, CaptureOutputNatif, FormatOuvert, PuitsNatif, TransformationsReelles,
};

// ── Les relevés (49ecf1fe) ─────────────────────────────────────────────────
//
// Route native, volume 1 000 (unité), DSP au repos : les mots poussés dans
// l'anneau sont `pcm_bytes_to_native_i32(source)`, l'identité.
const EMPREINTE_NATIVE_16_BITS_IDENTITE: u64 = 0xca50_4064_e842_bcf5;
const EMPREINTE_NATIVE_24_BITS_IDENTITE: u64 = 0x1688_08b2_7642_8f1e;
// Route native, DoP (fixture `dop_stereo_24le_64frames.hex`), volume 250 :
// le DoP force la branche brute, volume et DSP ne le touchent jamais.
const EMPREINTE_NATIVE_DOP: u64 = 0x12e6_821d_877b_2aa5;
// Route native, 16 bits, volume 500 : aller-retour flottant, quantifié UNE
// fois (`f32_to_native_i32`), avant l'anneau (D3).
const EMPREINTE_NATIVE_16_BITS_VOLUME_500: u64 = 0x9b8d_7155_baac_bb7c;
// Route traitée, 16 et 24 bits, DSP au repos : les `f32` de
// `pcm_bytes_to_f32`, sans volume — il est dans le rappel.
const EMPREINTE_FLOTTANTE_16_BITS: u64 = 0xbb0a_b2e4_f3bd_af91;
const EMPREINTE_FLOTTANTE_24_BITS: u64 = 0xe900_3ca2_aef3_dbdd;

/// La cadence des relevés. Elle n'entre dans aucune empreinte (aucun
/// rééchantillonnage sur ces routes), mais elle est celle des pilotes ASIO
/// de DEvir et jfpaquet.
const CADENCE: u32 = 44_100;
const TRAMES: usize = 256;

/// Un signal déterministe : une sinusoïde à −6 dB plus un bruit LCG à −60 dB,
/// pour que chaque octet compte et que les deux voies diffèrent.
fn signal_i32(profondeur_bits: u32) -> Vec<i32> {
    let pleine_echelle = (1i64 << (profondeur_bits - 1)) as f64;
    let mut lcg: u64 = 0x2545_F491_4F6C_DD1D;
    (0..TRAMES * 2)
        .map(|i| {
            lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let bruit = ((lcg >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 0.002;
            let trame = (i / 2) as f64;
            let voie = if i % 2 == 0 { 1.0 } else { 0.7 };
            let sinus = (2.0 * std::f64::consts::PI * 441.0 * trame / CADENCE as f64).sin();
            let v = (sinus * 0.5 * voie + bruit) * pleine_echelle;
            v.round().clamp(-pleine_echelle, pleine_echelle - 1.0) as i32
        })
        .collect()
}

/// Les octets source petit-boutistes du signal, en 16 ou 24 bits.
fn source(bit_depth: u16) -> Vec<u8> {
    let bps = usize::from(bit_depth / 8);
    signal_i32(u32::from(bit_depth))
        .iter()
        .flat_map(|v| v.to_le_bytes()[..bps].to_vec())
        .collect()
}

/// Le porteur DoP versionné : 64 trames stéréo 24 bits, marqueurs
/// 0x05/0xFA alternés — la sortie réelle de l'encodeur.
fn porteur_dop() -> Vec<u8> {
    include_str!("../../../tests/fixtures/dop_stereo_24le_64frames.hex")
        .split_ascii_whitespace()
        .map(|octet| u8::from_str_radix(octet, 16).expect("fixture DoP hex valide"))
        .collect()
}

fn stereo(bit_depth: u16) -> AudioSpec {
    AudioSpec::depuis_entete(CADENCE, bit_depth, 2).expect("format stéréo valide")
}

/// Le DSP au repos et les atomiques que les deux routes lisent.
struct Dsp {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    dop_active: AtomicBool,
    volume: AtomicU32,
    user_volume: AtomicU32,
    rg_factor: AtomicU32,
}

impl Dsp {
    fn au_repos(volume: u32) -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            dop_active: AtomicBool::new(false),
            volume: AtomicU32::new(volume),
            user_volume: AtomicU32::new(volume),
            rg_factor: AtomicU32::new(1000),
        }
    }

    /// La chaîne d'AVANT sur la route native : `prepare_windows_native_pcm`
    /// sur une fenêtre complète, puis les mots sérialisés comme
    /// `NativePcmRing` les tient — ce que `feed_windows_native_exclusive_leftover`
    /// poussait.
    fn avant_native(&self, octets: &[u8], spec: AudioSpec) -> (Vec<u8>, bool, bool) {
        let prepared = prepare_windows_native_pcm(
            octets,
            spec.profondeur().bits_declares(),
            spec.canaux(),
            spec.profondeur().bits_declares() == 24,
            false,
            self.volume.load(Ordering::SeqCst),
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
        .expect("fenêtre complète : au moins 32 trames");
        let mots: Vec<u8> = prepared
            .samples
            .iter()
            .flat_map(|mot| mot.to_le_bytes())
            .collect();
        (mots, prepared.dop, prepared.bit_perfect)
    }

    /// La chaîne d'APRÈS sur la route native : l'étage natif de REF-8.
    fn etage_natif(&self, spec: AudioSpec) -> EtageNatif<'_> {
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

    /// La chaîne d'AVANT sur la route traitée : `prepare_windows_exclusive_pcm`
    /// — quarantaine 24 bits, refus DoP, `pcm_bytes_to_f32`, DSP.
    fn avant_flottante(
        &self,
        octets: &[u8],
        spec: AudioSpec,
    ) -> Result<Option<Vec<f32>>, WindowsExclusivePcmError> {
        prepare_windows_exclusive_pcm(
            octets,
            spec.profondeur().bits_declares(),
            spec.canaux(),
            spec.profondeur().bits_declares() == 24,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
    }

    /// La chaîne d'APRÈS sur la route traitée : l'étage de R1, monté comme
    /// `bras_asio.rs` le monte — `sortie` = format source, pas de
    /// rééchantillonnage.
    fn etage_flottant(&self, octets: Vec<u8>, spec: AudioSpec) -> EtageDeConversion<'_> {
        EtageDeConversion {
            pcm: LocalPcmProcessor {
                eq: &self.eq,
                convolver: &self.convolver,
                crossfeed: &self.crossfeed,
                pure_bypass: &self.pure_bypass,
                mono_downmix: &self.mono_downmix,
                dop_active: &self.dop_active,
                volume: &self.volume,
                user_volume: &self.user_volume,
                rg_factor: &self.rg_factor,
            },
            en_attente: octets,
            resampler: None,
            resample_leftover: Vec::new(),
            pcm_kind: LocalPcmKind::for_bit_depth(spec.profondeur().bits_declares()),
            spec,
            sortie: FormatOuvert::new(spec.cadence(), spec.canaux()),
            needs_resample: false,
        }
    }
}

/// L'empreinte d'un bloc natif tel que l'anneau le tenait.
fn empreinte_des_mots(spec: AudioSpec, mots: &[u8]) -> u64 {
    let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
    assert!(capture.ecrire(spec_du_puits_natif(spec).bloc(mots)));
    capture.empreinte()
}

/// L'empreinte de `f32` tels que l'anneau flottant les tenait.
fn empreinte_des_f32(spec: AudioSpec, mots: &[f32]) -> u64 {
    let mut capture = CaptureOutput::ouvert(FormatOuvert::new(spec.cadence(), spec.canaux()));
    assert!(capture.ecrire(mots));
    capture.empreinte()
}

/// Route native, identité : AVANT = mots source, APRÈS = les mêmes, par
/// l'étage natif dans un puits de capture ; l'empreinte est le relevé.
fn route_native_identite(bit_depth: u16, releve: u64) {
    let spec = stereo(bit_depth);
    let octets = source(bit_depth);
    let dsp = Dsp::au_repos(1000);

    let (avant, dop, bit_perfect) = dsp.avant_native(&octets, spec);
    assert!(
        !dop && bit_perfect,
        "{bit_depth} bits au repos : branche brute"
    );
    let attendu: Vec<u8> = pcm_bytes_to_native_i32(&octets, bit_depth)
        .iter()
        .flat_map(|mot| mot.to_le_bytes())
        .collect();
    assert_eq!(avant, attendu, "la branche brute est l'identité");
    let empreinte_avant = empreinte_des_mots(spec, &avant);

    let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
    let mut etage = dsp.etage_natif(spec);
    assert!(matches!(
        etage.decoder_et_pousser(&octets, &mut capture),
        EcritureNative::Poussee {
            trames_source,
            dop: false,
            bit_perfect: true
        } if trames_source == TRAMES as u64
    ));
    assert_eq!(capture.trames(), TRAMES as u64);
    assert_eq!(
        capture.empreinte(),
        empreinte_avant,
        "l'étage natif ne pousse pas ce que feed_windows_native_exclusive_leftover poussait"
    );
    assert_eq!(
        capture.empreinte(),
        releve,
        "relevé {bit_depth} bits identité (49ecf1fe) : mesuré {:#018x}",
        capture.empreinte()
    );
}

#[test]
fn route_native_16_bits_identite_rend_les_mots_source() {
    route_native_identite(16, EMPREINTE_NATIVE_16_BITS_IDENTITE);
}

#[test]
fn route_native_24_bits_identite_rend_les_mots_source() {
    route_native_identite(24, EMPREINTE_NATIVE_24_BITS_IDENTITE);
}

/// Le DoP force la branche brute quel que soit le volume : chaque marqueur
/// 0x05/0xFA et chaque octet de charge utile traversent l'étage intacts.
#[test]
fn route_native_porte_le_dop_intact_meme_a_volume_reduit() {
    let spec = stereo(24);
    let octets = porteur_dop();
    let dsp = Dsp::au_repos(250);

    let (avant, dop, bit_perfect) = dsp.avant_native(&octets, spec);
    assert!(dop && bit_perfect, "un porteur DoP prend la branche brute");
    let empreinte_avant = empreinte_des_mots(spec, &avant);

    let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
    let mut etage = dsp.etage_natif(spec);
    assert!(matches!(
        etage.decoder_et_pousser(&octets, &mut capture),
        EcritureNative::Poussee {
            trames_source: 64,
            dop: true,
            bit_perfect: true
        }
    ));
    assert!(etage.dop_verrouille());
    assert_eq!(capture.empreinte(), empreinte_avant);
    assert_eq!(
        capture.empreinte(),
        EMPREINTE_NATIVE_DOP,
        "relevé DoP (49ecf1fe) : mesuré {:#018x}",
        capture.empreinte()
    );

    // Et les octets sont bien ceux de la source, marqueurs compris : le mot
    // natif d'un échantillon 24 bits est son octet haut en bits 31..24.
    let mots = pcm_bytes_to_native_i32(&octets, 24);
    for (trame, paire) in mots.chunks_exact(2).enumerate() {
        let marqueur = if trame % 2 == 0 { 0x05 } else { 0xFA };
        assert_eq!((paire[0] >> 24) as u8, marqueur);
        assert_eq!((paire[1] >> 24) as u8, marqueur);
    }
}

/// Volume à 50 % sur la route native : l'aller-retour flottant est fait UNE
/// fois avant l'anneau, et le verdict le dit (`bit_perfect = false`).
#[test]
fn route_native_a_volume_reduit_quantifie_une_fois_avant_l_anneau() {
    let spec = stereo(16);
    let octets = source(16);
    let dsp = Dsp::au_repos(500);

    let (avant, dop, bit_perfect) = dsp.avant_native(&octets, spec);
    assert!(!dop && !bit_perfect);
    let empreinte_avant = empreinte_des_mots(spec, &avant);
    assert_ne!(
        empreinte_avant, EMPREINTE_NATIVE_16_BITS_IDENTITE,
        "le volume change les mots, ou il n'est pas appliqué"
    );

    let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
    let mut etage = dsp.etage_natif(spec);
    assert!(matches!(
        etage.decoder_et_pousser(&octets, &mut capture),
        EcritureNative::Poussee {
            dop: false,
            bit_perfect: false,
            ..
        }
    ));
    assert_eq!(capture.empreinte(), empreinte_avant);
    assert_eq!(
        capture.empreinte(),
        EMPREINTE_NATIVE_16_BITS_VOLUME_500,
        "relevé 16 bits volume 500 (49ecf1fe) : mesuré {:#018x}",
        capture.empreinte()
    );
    let transformations: TransformationsReelles = etage.transformations();
    assert!(!transformations.reechantillonnage());
}

/// Route traitée : AVANT = `prepare_windows_exclusive_pcm`, APRÈS = l'étage
/// de R1 avec la fermeture qui refuse tout porteur. Mêmes `f32`.
fn route_traitee_identite(bit_depth: u16, releve: u64) {
    let spec = stereo(bit_depth);
    let octets = source(bit_depth);
    let dsp = Dsp::au_repos(1000);

    let avant = dsp
        .avant_flottante(&octets, spec)
        .expect("PCM ordinaire : accepté")
        .expect("fenêtre complète");
    assert_eq!(
        avant,
        pcm_bytes_to_f32(&octets, bit_depth),
        "au repos, la route traitée rend pcm_bytes_to_f32 tel quel"
    );
    let empreinte_avant = empreinte_des_f32(spec, &avant);

    let mut capture = CaptureOutput::ouvert(FormatOuvert::new(CADENCE, 2));
    let mut etage = dsp.etage_flottant(octets, spec);
    assert!(matches!(
        etage.pousser(&mut capture, &mut |dop, _, _| dop, &mut |_| {}),
        PousseeVersLePuits::Poussee { trames_source } if trames_source == TRAMES as u64
    ));
    assert_eq!(capture.trames(), TRAMES as u64);
    assert_eq!(
        capture.empreinte(),
        empreinte_avant,
        "l'étage de R1 ne rend pas ce que prepare_windows_exclusive_pcm rendait"
    );
    assert_eq!(
        capture.empreinte(),
        releve,
        "relevé route traitée {bit_depth} bits (49ecf1fe) : mesuré {:#018x}",
        capture.empreinte()
    );
    assert!(!dsp.dop_active.load(Ordering::SeqCst));
}

#[test]
fn route_traitee_16_bits_par_l_etage_de_r1_rend_les_memes_f32() {
    route_traitee_identite(16, EMPREINTE_FLOTTANTE_16_BITS);
}

#[test]
fn route_traitee_24_bits_par_l_etage_de_r1_rend_les_memes_f32() {
    route_traitee_identite(24, EMPREINTE_FLOTTANTE_24_BITS);
}

/// Le refus DoP de la route traitée SURVIT à la migration : AVANT
/// `DopUnsupported` avant conversion, APRÈS `PorteurDopRefuse` avant
/// conversion — et rien n'atteint le puits dans les deux cas.
#[test]
fn route_traitee_refuse_le_porteur_dop_avant_toute_conversion() {
    let spec = stereo(24);
    let octets = porteur_dop();
    let dsp = Dsp::au_repos(1000);

    assert_eq!(
        dsp.avant_flottante(&octets, spec),
        Err(WindowsExclusivePcmError::DopUnsupported)
    );

    let mut capture = CaptureOutput::ouvert(FormatOuvert::new(CADENCE, 2));
    let mut etage = dsp.etage_flottant(octets, spec);
    assert!(matches!(
        etage.pousser(&mut capture, &mut |dop, _, _| dop, &mut |_| {}),
        PousseeVersLePuits::PorteurDopRefuse
    ));
    assert_eq!(
        capture.mots(),
        0,
        "rien ne doit atteindre l'anneau flottant"
    );
    // `process_pcm_chunk` a levé `dop_active` en classant le flux : le bras
    // le rabaisse sur le chemin du refus, comme avant
    // (`dop_active.store(false)` + `sync_volume_to_dop(false)`).
    assert!(dsp.dop_active.load(Ordering::SeqCst));
}

/// 31 trames 24 bits ne prouvent ni PCM ni DoP : quarantaine sur les deux
/// chaînes, et à l'EOF la sonde incomplète reste un refus (`DopCheckIncomplete`).
#[test]
fn route_traitee_quarantaine_puis_refus_a_l_eof_si_la_sonde_est_incomplete() {
    let spec = stereo(24);
    let octets = porteur_dop()[..31 * 6].to_vec();
    let dsp = Dsp::au_repos(1000);

    assert!(matches!(dsp.avant_flottante(&octets, spec), Ok(None)));

    let mut capture = CaptureOutput::ouvert(FormatOuvert::new(CADENCE, 2));
    let mut etage = dsp.etage_flottant(octets.clone(), spec);
    assert!(matches!(
        etage.pousser(&mut capture, &mut |dop, _, _| dop, &mut |_| {}),
        PousseeVersLePuits::RienAPousser
    ));
    assert_eq!(capture.mots(), 0);
    assert!(etage.pcm_kind.is_awaiting_probe());
    assert_eq!(etage.en_attente.len(), octets.len());
    // Ce que `Route::vider` demande à l'EOF sur la route traitée.
    assert_eq!(
        finish_windows_exclusive_probe(
            24,
            etage.pcm_kind.is_awaiting_probe(),
            etage.en_attente.len()
        ),
        Err(WindowsExclusivePcmError::DopCheckIncomplete)
    );
}
