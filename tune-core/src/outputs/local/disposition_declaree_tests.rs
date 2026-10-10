//! #6057 — l'étage de conversion du chemin partagé route par la disposition
//! que le fichier déclare, et, sans déclaration, par l'ordre par défaut.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::{CreneauDisposition, EtageDeConversion, LocalPcmKind, LocalPcmProcessor};
use crate::audio::disposition_canaux::{Disposition, depuis_dsf};
use crate::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};

fn convertir(
    disposition: Option<Disposition>,
    dop: bool,
    entree: Vec<f32>,
    sortie: u16,
) -> Vec<f32> {
    let creneau: CreneauDisposition = std::sync::Mutex::new(disposition.map(Arc::new));
    let (eq, convolver, crossfeed) = (
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
    );
    let (pure, mono, dop_active) = (
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    );
    dop_active.store(dop, Ordering::Relaxed);
    let (v, uv, rg) = (
        AtomicU32::new(100),
        AtomicU32::new(100),
        AtomicU32::new(100),
    );
    let mut etage = EtageDeConversion {
        pcm: LocalPcmProcessor {
            eq: &eq,
            convolver: &convolver,
            crossfeed: &crossfeed,
            pure_bypass: &pure,
            mono_downmix: &mono,
            disposition: &creneau,
            dop_active: &dop_active,
            volume: &v,
            user_volume: &uv,
            rg_factor: &rg,
        },
        en_attente: Vec::new(),
        resampler: None,
        resample_leftover: Vec::new(),
        pcm_kind: LocalPcmKind::for_bit_depth(24),
        spec: AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier24, 4).expect("spec"),
        sortie: FormatOuvert::new(48_000, sortie),
        needs_resample: false,
    };
    etage.convertir(entree)
}

/// Sans déclaration, un 4 canaux est un 4.0 (FL FR BL BR) : vers 6 voies, ses
/// arrière vont sur les arrière, jamais sur le centre ni le LFE.
#[test]
fn un_4_0_vers_6_voies_garde_ses_arriere_a_l_arriere() {
    assert_eq!(
        convertir(None, false, vec![0.125, 0.25, 0.375, 0.5], 6),
        [0.125, 0.25, 0.0, 0.0, 0.375, 0.5],
        "BL/BR doivent sortir sur BL/BR, pas sur FC/LFE"
    );
}

/// Déclaré 3.1 (DSF type 5 : FL FR FC LFE), le même flux garde son centre et
/// son LFE à leur place.
#[test]
fn un_3_1_declare_vers_6_voies_garde_centre_et_lfe() {
    assert_eq!(
        convertir(depuis_dsf(5, 4), false, vec![0.125, 0.25, 0.375, 0.5], 6),
        [0.125, 0.25, 0.375, 0.5, 0.0, 0.0],
        "la disposition déclarée par le fichier doit l'emporter sur l'ordre par défaut"
    );
}

/// Un porteur DoP n'est jamais routé par la disposition déclarée.
#[test]
fn un_porteur_dop_garde_l_adaptation_d_avant() {
    assert_eq!(
        convertir(depuis_dsf(5, 4), true, vec![0.125, 0.25, 0.375, 0.5], 6),
        [0.125, 0.25, 0.0, 0.0, 0.375, 0.5]
    );
}
