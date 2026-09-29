//! #5416 — sortie qui convertit DÉJÀ la piste courante (WASAPI partagé à
//! 192 kHz), zone en PURE : chaque passage 44,1 → 44,1 était refusé pour
//! rouvrir, l'anneau se vidait en fin de piste et la suivante repartait
//! après un blanc.
//!
//! Didier, fil 2022, Tune 0.9.167, SMSL SU-8 (journal `l1126.txt`) :
//!
//! ```text
//! 09:38:03.520 local_audio_gapless_rate_change_reopen requested_sr=44100 stream_sr=192000 motif="pure"
//! 09:38:05.565 famine_anneau_debut … silence_ms=11
//! 09:38:05.572 local_audio_stopped
//! 09:38:06.060 output_play_sent
//! ```
//!
//! Rouvrir ne pouvait rien changer : le périphérique repart à 192 kHz et la
//! piste est convertie de la même façon. Le témoin joue la frontière réelle de
//! la boucle gapless (`EtageDeConversion::enchainer_la_piste`) sur un puits
//! factice.

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::*;
use crate::outputs::traits::CaptureOutput;

fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// La zone de Didier : PURE allumé, périphérique qui ne suit pas la source.
const PURE_SEUL: ReglesDeCadence = ReglesDeCadence {
    strict: false,
    pure: true,
    peripherique_suit_la_source: false,
};

/// Le flux ouvert à 192 kHz stéréo, la piste courante à 44,1 kHz 32 bits
/// déjà convertie, avec un rééchantillonneur vivant et un reliquat en cours
/// — l'état d'une lecture en WASAPI partagé au moment où l'en-tête suivant
/// arrive.
fn etage_44k_converti_en_192k(dsp: &DspAuRepos) -> EtageDeConversion<'_> {
    let mut e = etage(dsp, Vec::new(), 44_100, 2, 32, 192_000, 2);
    assert!(
        e.needs_resample,
        "44,1 kHz vers 192 kHz : conversion en cours"
    );
    e.resampler = Some(
        crate::audio::resample::new_streaming_resampler(44_100, 192_000, 2)
            .expect("rééchantillonneur 44,1 → 192 kHz"),
    );
    e.resample_leftover = vec![0.25; 64];
    e
}

fn piste_44k_32b() -> AudioSpec {
    AudioSpec::depuis_entete(44_100, 32, 2).expect("format valide")
}

#[test]
fn chemin_gapless_5416_pure_meme_source_deja_convertie_enchaine_sans_rouvrir() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage_44k_converti_en_192k(&dsp);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(192_000, 2));

    let verdict = e.enchainer_la_piste(&mut puits, piste_44k_32b(), PURE_SEUL, "SMSL SU-8");

    assert_eq!(
        verdict,
        Ok(false),
        "PURE, piste 44,1 kHz enchaînée sur une piste 44,1 kHz déjà convertie \
         vers un flux à 192 kHz : l'enchaînement a été refusé pour rouvrir, \
         ce qui vide l'anneau en fin de piste (#5416)"
    );
    assert!(
        e.needs_resample && e.resampler.is_some(),
        "la conversion continue sur la piste enchaînée"
    );
    assert_eq!(
        e.resample_leftover.len(),
        64,
        "le reliquat du rééchantillonneur fait partie du flux continu : \
         ni vidé ni remis à zéro"
    );
    assert_eq!(
        puits.mots(),
        0,
        "rien n'a été vidé au puits à la frontière : pas de queue de \
         rééchantillonneur entre deux pistes à la même cadence"
    );
    assert_eq!(e.sample_rate(), 44_100);
}

#[test]
fn chemin_gapless_5416_le_strict_rouvre_toujours() {
    let dsp = DspAuRepos::neuf();
    for regles in [
        ReglesDeCadence {
            strict: true,
            ..ReglesDeCadence::default()
        },
        ReglesDeCadence {
            strict: true,
            pure: true,
            peripherique_suit_la_source: false,
        },
    ] {
        let mut e = etage_44k_converti_en_192k(&dsp);
        let mut puits = CaptureOutput::ouvert(FormatOuvert::new(192_000, 2));
        assert_eq!(
            e.enchainer_la_piste(&mut puits, piste_44k_32b(), regles, "SMSL SU-8"),
            Err(MotifDeReouverture::BitPerfectStrict),
            "bit-perfect strict : la règle de #3973 est inchangée ({regles:?})"
        );
        assert_eq!(puits.mots(), 0, "rien n'a été écrit au puits");
        assert_eq!(e.resample_leftover.len(), 64, "l'étage n'a pas été touché");
    }
}

/// Une AUTRE cadence source reste l'affaire de #4953 : PURE rouvre.
#[test]
fn chemin_gapless_5416_pure_autre_cadence_rouvre_comme_4953() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage_44k_converti_en_192k(&dsp);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(192_000, 2));
    let piste_48k = AudioSpec::depuis_entete(48_000, 32, 2).expect("format valide");

    assert_eq!(
        e.enchainer_la_piste(&mut puits, piste_48k, PURE_SEUL, "SMSL SU-8"),
        Err(MotifDeReouverture::Pure)
    );
}

/// Une piste courante NON convertie (le périphérique a suivi la source) ne
/// profite pas du raccourci : 48 kHz sur un flux à 44,1 kHz rouvre toujours.
#[test]
fn chemin_gapless_5416_sans_conversion_en_cours_rien_ne_change() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, Vec::new(), 44_100, 2, 16, 44_100, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));
    let piste_48k = AudioSpec::depuis_entete(48_000, 16, 2).expect("format valide");

    assert_eq!(
        e.enchainer_la_piste(&mut puits, piste_48k, PURE_SEUL, "DENAFRIPS"),
        Err(MotifDeReouverture::Pure)
    );
}

/// Garde du branchement : le raccourci vit dans la frontière, exclut le
/// strict, et tranche AVANT la règle de #4953 — qui reste consultée.
#[test]
fn branchement_5416_le_raccourci_exclut_le_strict_et_precede_la_regle() {
    let src = compact(include_str!("../local.rs"));
    let corps = src
        .find("fnenchainer_la_piste(")
        .expect("la frontière de l'étage");
    let raccourci = src[corps..]
        .find(
            "letmeme_source_deja_convertie=!regles.strict&&self.needs_resample&&new_sr==self.sample_rate();",
        )
        .expect("le raccourci #5416 exclut le strict et exige une conversion en cours");
    let regle = src[corps..]
        .find("decider_la_cadence_enchainee(new_sr,output_sr,regles)")
        .expect("la règle de #4953 reste consultée");
    assert!(raccourci < regle);
}
