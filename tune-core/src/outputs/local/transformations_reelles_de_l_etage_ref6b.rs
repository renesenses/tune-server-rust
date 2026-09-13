//! REF-6b côté producteur (#2219) — l'étage dit ce qu'il fait, et
//! `LocalOutput` le publie.
//!
//! #3987 a posé le contrat (`TransformationsReelles`, défaut `None`) et le
//! consommateur (le chemin du signal préfère la mesure à la déduction), avec
//! quatre témoins qui POSENT une mesure à la main dans `ZoneState`. Il manquait
//! le producteur : personne, dans la sortie locale, ne remplissait jamais ce
//! contrat — la route affichait toujours sa déduction.
//!
//! Ce module tient les deux bouts du producteur :
//!
//! * `EtageDeConversion::transformations()` rend EXACTEMENT ce que l'étage
//!   fait — spec d'entrée, format ouvert, DSP posé — et c'est cette valeur,
//!   au bit près, que le témoin de route de `signal_path_tests.rs`
//!   (`une_zone_locale_ouverte_a_48_khz_sur_une_source_44_1_rend_le_reechantillonnage_mesure`)
//!   pousse dans la route pour obtenir `bit_perfect = false` et l'étape
//!   « 44kHz → 48kHz (mesuré) » ;
//! * `LocalOutput::transformations_reelles()` rend `None` tant qu'aucun étage
//!   n'a publié, puis ce que `publier_les_transformations` y a posé.
//!
//! Les SITES de publication (ouverture, frontière gapless) sont gardés par
//! texte dans `signal_path_tests.rs`, qui tourne dans le job `Test` de chaque
//! PR ; ce module-ci vit derrière `local-audio` et n'est joué que sous
//! `ci:full` et sur Shrek.

use std::sync::atomic::{AtomicBool, AtomicU32};

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::{
    Etage, EtageDeConversion, LocalOutput, LocalPcmKind, LocalPcmProcessor,
    publier_les_transformations,
};
use crate::outputs::traits::{
    AudioSpec, FormatOuvert, OutputTarget, ProfondeurPcm, TransformationsReelles,
};

/// La mesure attendue pour une source 44,1 kHz / 16 bits / stéréo servie sur
/// un périphérique ouvert à 48 kHz stéréo, sans DSP — la même valeur que le
/// témoin de route construit de son côté.
fn attendue_44_1_vers_48() -> TransformationsReelles {
    TransformationsReelles::nouvelles(
        AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).expect("spec valide"),
        FormatOuvert::new(48_000, 2),
        false,
    )
}

#[test]
fn une_source_44_1_sur_un_peripherique_ouvert_a_48_declare_le_reechantillonnage() {
    let dsp = DspAuRepos::neuf();
    let e = etage(&dsp, Vec::new(), 44_100, 2, 16, 48_000, 2);
    let t = e.transformations();
    assert_eq!(
        t,
        attendue_44_1_vers_48(),
        "l'étage doit déclarer sa spec d'entrée, son format ouvert et l'absence de DSP, \
         tels quels"
    );
    assert!(t.reechantillonnage(), "44,1 -> 48 kHz : rééchantillonné");
    assert!(
        !t.adaptation_canaux(),
        "stéréo -> stéréo : pas d'adaptation"
    );
    assert!(!t.dsp_actif(), "aucun traitement posé");
}

#[test]
fn a_format_identique_l_etage_ne_declare_aucune_transformation() {
    let dsp = DspAuRepos::neuf();
    let e = etage(&dsp, Vec::new(), 96_000, 2, 24, 96_000, 2);
    let t = e.transformations();
    assert!(!t.reechantillonnage());
    assert!(!t.adaptation_canaux());
    assert!(!t.dsp_actif());
    assert_eq!(t.entree().cadence(), 96_000);
    assert_eq!(t.ouvert(), FormatOuvert::new(96_000, 2));
}

#[test]
fn une_adaptation_de_canaux_est_declaree_depuis_le_format_ouvert() {
    let dsp = DspAuRepos::neuf();
    let e = etage(&dsp, Vec::new(), 48_000, 2, 16, 48_000, 8);
    let t = e.transformations();
    assert!(t.adaptation_canaux(), "stéréo -> 8 canaux : adapté");
    assert!(!t.reechantillonnage());
}

/// Le DSP « posé » se lit comme `apply_local_dsp` le décide : le repli mono
/// sur une source stéréo est un traitement ; le même repli sur une source
/// mono n'en est pas un (la chaîne ne l'applique qu'à deux canaux).
struct DspAvecRepliMono {
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

impl DspAvecRepliMono {
    fn neuf() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(true),
            dop_active: AtomicBool::new(false),
            volume: AtomicU32::new(100),
            user_volume: AtomicU32::new(100),
            rg_factor: AtomicU32::new(100),
        }
    }

    fn etage(&self, canaux: u16) -> EtageDeConversion<'_> {
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
            en_attente: Vec::new(),
            resampler: None,
            resample_leftover: Vec::new(),
            pcm_kind: LocalPcmKind::for_bit_depth(16),
            spec: AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, canaux).expect("spec"),
            sortie: FormatOuvert::new(48_000, canaux),
            needs_resample: false,
        }
    }
}

#[test]
fn le_repli_mono_est_un_dsp_actif_sur_une_source_stereo_seulement() {
    let dsp = DspAvecRepliMono::neuf();
    assert!(
        dsp.etage(2).transformations().dsp_actif(),
        "repli mono posé, source stéréo : la chaîne le joue, l'étage doit le dire"
    );
    assert!(
        !dsp.etage(1).transformations().dsp_actif(),
        "repli mono posé, source mono : la chaîne ne l'applique pas, l'étage ne doit pas \
         annoncer un traitement qui n'a pas lieu"
    );
    dsp.pure_bypass
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        !dsp.etage(2).transformations().dsp_actif(),
        "contournement pur : rien n'est traité, quoi qu'il y ait de posé"
    );
}

#[test]
fn local_output_rend_none_avant_toute_publication_puis_ce_que_l_etage_declare() {
    let sortie = LocalOutput::new("témoin REF-6b".into());
    assert_eq!(
        sortie.transformations_reelles(),
        None,
        "hors lecture, le contrat rend `None` : le chemin du signal garde sa déduction"
    );

    let dsp = DspAuRepos::neuf();
    let e = etage(&dsp, Vec::new(), 44_100, 2, 16, 48_000, 2);
    publier_les_transformations(&sortie.transformations_reelles, &e);
    assert_eq!(
        sortie.transformations_reelles(),
        Some(attendue_44_1_vers_48()),
        "ce que le fil de lecture publie est ce que le sondeur lit"
    );

    // Une frontière gapless vers une autre source publie la NOUVELLE entrée.
    let e2 = etage(&dsp, Vec::new(), 48_000, 2, 24, 48_000, 2);
    publier_les_transformations(&sortie.transformations_reelles, &e2);
    let t = sortie.transformations_reelles().expect("publiée");
    assert!(
        !t.reechantillonnage(),
        "48 -> 48 : la piste enchaînée n'est plus rééchantillonnée"
    );
    assert_eq!(t.entree().cadence(), 48_000);
}
