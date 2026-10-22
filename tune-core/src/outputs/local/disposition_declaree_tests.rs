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
            reaffectation: &crate::outputs::local::reaffectation::SANS_REAFFECTATION,
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

/// L'ordre `CHNL` d'un DFF 5.1 qui n'est pas celui de la sortie :
/// `C LFE SLFT SRGT LS RS` (FC LFE FL FR BL BR).
fn dff_5_1_hors_ordre() -> Option<Disposition> {
    crate::audio::disposition_canaux::depuis_dff(&[
        *b"C   ", *b"LFE ", *b"SLFT", *b"SRGT", *b"LS  ", *b"RS  ",
    ])
}

/// Ce que l'étage du chemin partagé déclare (`transformations`) et ce qu'il
/// fait (`convertir`) pour une source 6 canaux ouverte en 6 voies.
fn etage_6_vers_6(
    disposition: Option<Disposition>,
    dop: bool,
    pure: bool,
) -> (crate::outputs::traits::TransformationsReelles, Vec<f32>) {
    let creneau: CreneauDisposition = std::sync::Mutex::new(disposition.map(Arc::new));
    let (eq, convolver, crossfeed) = (
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
    );
    let (pure_bypass, mono, dop_active) = (
        AtomicBool::new(pure),
        AtomicBool::new(false),
        AtomicBool::new(dop),
    );
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
            pure_bypass: &pure_bypass,
            mono_downmix: &mono,
            disposition: &creneau,
            reaffectation: &crate::outputs::local::reaffectation::SANS_REAFFECTATION,
            dop_active: &dop_active,
            volume: &v,
            user_volume: &uv,
            rg_factor: &rg,
        },
        en_attente: Vec::new(),
        resampler: None,
        resample_leftover: Vec::new(),
        pcm_kind: LocalPcmKind::for_bit_depth(24),
        spec: AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier24, 6).expect("spec"),
        sortie: FormatOuvert::new(48_000, 6),
        needs_resample: false,
    };
    let t = super::Etage::transformations(&etage);
    let rendu = etage.convertir(vec![0.125, 0.25, 0.375, 0.5, 0.625, 0.75]);
    (t, rendu)
}

/// #6057 — point restant de #6058 : à nombre de canaux égal, l'étage
/// RÉORDONNE les voies d'un DFF hors ordre (FC LFE FL FR BL BR vers FL FR FC
/// LFE BL BR), mais ne le déclarait pas. Le chemin du signal disait « 6
/// canaux, sortie multicanal », bit-perfect. L'étage doit le déclarer dans
/// ses transformations réelles, et seulement quand il le fait.
#[test]
fn une_reorganisation_a_nombre_egal_est_declaree_par_l_etage_6057() {
    let (t, rendu) = etage_6_vers_6(dff_5_1_hors_ordre(), false, false);
    assert_eq!(
        rendu,
        [0.375, 0.5, 0.125, 0.25, 0.625, 0.75],
        "prémisse : l'étage réordonne bien les voies du DFF"
    );
    assert!(
        t.reorganisation_canaux(),
        "l'étage réordonne les voies à nombre égal : il doit le déclarer (#6057)"
    );
    assert!(!t.adaptation_canaux(), "six voies entrées, six ouvertes");

    // Sans déclaration, ou l'ordre par défaut déclaré : rien ne bouge, rien
    // n'est déclaré.
    for (disposition, cas) in [
        (None, "sans déclaration"),
        (Disposition::par_defaut(6), "ordre par défaut déclaré"),
    ] {
        let (t, rendu) = etage_6_vers_6(disposition, false, false);
        assert_eq!(rendu, [0.125, 0.25, 0.375, 0.5, 0.625, 0.75], "{cas}");
        assert!(!t.reorganisation_canaux(), "{cas} : aucune réorganisation");
    }
    // Un porteur DoP n'est jamais réordonné : rien à déclarer.
    let (t, _) = etage_6_vers_6(dff_5_1_hors_ordre(), true, false);
    assert!(!t.reorganisation_canaux(), "DoP : rien n'est réordonné");
}
