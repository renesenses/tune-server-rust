//! #4953 — une piste enchaînée en gapless à une AUTRE cadence que le flux
//! ouvert était convertie en silence au lieu de rouvrir le périphérique.
//!
//! Belkadi Yacine, fil 1922 (v0.9.163, DENAFRIPS en `hw:`, PURE allumé) :
//!
//! ```text
//! 18:47:47.348 local_audio_gapless_next_track_format new_sr=48000 … prev_sr=44100
//! 18:47:47.448 local_audio_gapless_resampler_recreated from_sr=48000 to_sr=44100
//! ```
//!
//! Aucune carte son n'est joignable depuis la machine de test. Trois étages de
//! témoins, donc :
//!
//! 1. **la décision**, fonction pure, éprouvée par valeurs ;
//! 2. **le chemin réel** : la frontière de l'étage que la boucle gapless de
//!    `play_url` appelle (`EtageDeConversion::enchainer_la_piste`), jouée sur
//!    un puits factice ([`CaptureOutput`]) — c'est elle qui créait le
//!    rééchantillonneur du relevé ;
//! 3. **le branchement** : la boucle gapless et l'ouverture passent bien ces
//!    entrées-là à cette frontière-là, et la boucle n'a plus d'autre copie de
//!    la conversion.

use super::backend::suit_la_cadence_source;
use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::*;
use crate::outputs::traits::CaptureOutput;

fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Un DAC en `hw:` ouvert à la cadence de la source : ce que le journal de
/// Belkadi montre à chaque lancement de piste.
const DAC_QUI_SUIT: ReglesDeCadence = ReglesDeCadence {
    strict: false,
    pure: false,
    peripherique_suit_la_source: true,
};

const PURE_SEUL: ReglesDeCadence = ReglesDeCadence {
    strict: false,
    pure: true,
    peripherique_suit_la_source: false,
};

// ── 1. La décision ─────────────────────────────────────────────────────────

#[test]
fn decision_4953_un_dac_qui_suit_la_source_est_rouvert_pas_converti() {
    assert_eq!(
        decider_la_cadence_enchainee(48_000, 44_100, DAC_QUI_SUIT),
        CadenceEnchainee::Rouvrir(MotifDeReouverture::PeripheriqueSuitLaSource),
        "48 kHz enchaînée sur un flux ouvert à 44,1 kHz, DAC qui suit la source : \
         il faut rouvrir, pas rééchantillonner (#4953)"
    );
    assert_eq!(
        decider_la_cadence_enchainee(44_100, 48_000, DAC_QUI_SUIT),
        CadenceEnchainee::Rouvrir(MotifDeReouverture::PeripheriqueSuitLaSource),
        "le sens 44,1 → 48 du même relevé (16:15:05)"
    );
}

#[test]
fn decision_4953_pure_rouvre_meme_quand_le_peripherique_ne_suit_pas() {
    assert_eq!(
        decider_la_cadence_enchainee(48_000, 44_100, PURE_SEUL),
        CadenceEnchainee::Rouvrir(MotifDeReouverture::Pure),
        "PURE : une conversion de fréquence ne se fait pas en silence en gapless"
    );
}

#[test]
fn decision_4953_la_conversion_reste_quand_rien_ne_demande_le_bit_perfect() {
    // Périphérique qui convertit de toute façon (greffon, WASAPI partagé,
    // DAC qui ne tient pas la cadence), zone ni PURE ni stricte : garder
    // l'enchaînement sans blanc et convertir, comme avant.
    assert_eq!(
        decider_la_cadence_enchainee(48_000, 44_100, ReglesDeCadence::default()),
        CadenceEnchainee::Convertir
    );
}

#[test]
fn decision_4953_meme_cadence_rien_a_decider_quels_que_soient_les_reglages() {
    for regles in [
        ReglesDeCadence::default(),
        DAC_QUI_SUIT,
        PURE_SEUL,
        ReglesDeCadence {
            strict: true,
            pure: true,
            peripherique_suit_la_source: true,
        },
    ] {
        assert_eq!(
            decider_la_cadence_enchainee(44_100, 44_100, regles),
            CadenceEnchainee::MemeCadence,
            "{regles:?}"
        );
    }
}

#[test]
fn decision_4953_le_strict_garde_son_motif_et_sa_ligne() {
    let tout = ReglesDeCadence {
        strict: true,
        pure: true,
        peripherique_suit_la_source: true,
    };
    assert_eq!(
        decider_la_cadence_enchainee(96_000, 44_100, tout),
        CadenceEnchainee::Rouvrir(MotifDeReouverture::BitPerfectStrict),
        "le strict passe en premier : sa ligne de journal (#3973) est celle qu'on cherche"
    );
}

#[test]
fn ouverture_4953_suivre_la_source_exige_une_mesure_et_la_cadence_ouverte() {
    assert!(
        suit_la_cadence_source(true, 44_100, 44_100),
        "hw: à la source"
    );
    assert!(
        !suit_la_cadence_source(false, 44_100, 44_100),
        "un greffon (dmix, pulse) à la bonne cadence ne prouve rien"
    );
    assert!(
        !suit_la_cadence_source(true, 44_100, 48_000),
        "ouvert à une autre cadence : le périphérique a déjà refusé la source"
    );
    assert!(!suit_la_cadence_source(true, 0, 0), "cadence inconnue");
}

// ── 2. Le chemin réel, sur un puits factice ───────────────────────────────

/// Le flux ouvert à 44,1 kHz stéréo 16 bits ; la piste enchaînée à 48 kHz.
fn flux_44k_et_piste_48k() -> (DspAuRepos, AudioSpec) {
    let piste_48k = AudioSpec::depuis_entete(48_000, 16, 2).expect("format valide");
    (DspAuRepos::neuf(), piste_48k)
}

#[test]
fn chemin_gapless_4953_dac_qui_suit_la_source_n_enchaine_pas_et_ne_touche_a_rien() {
    let (dsp, piste_48k) = flux_44k_et_piste_48k();
    let mut e = etage(&dsp, Vec::new(), 44_100, 2, 16, 44_100, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));

    let verdict = e.enchainer_la_piste(&mut puits, piste_48k, DAC_QUI_SUIT, "DENAFRIPS");

    assert_eq!(
        verdict,
        Err(MotifDeReouverture::PeripheriqueSuitLaSource),
        "la frontière gapless a enchaîné une piste 48 kHz sur un flux 44,1 kHz \
         au lieu de rendre la main pour rouvrir le DAC (#4953)"
    );
    assert!(
        e.resampler.is_none() && !e.needs_resample,
        "aucun rééchantillonneur ne doit être créé pour la piste enchaînée"
    );
    assert_eq!(
        e.sample_rate(),
        44_100,
        "l'étage garde le format de la piste qui se termine"
    );
    assert_eq!(puits.mots(), 0, "rien n'a été écrit au puits");
}

#[test]
fn chemin_gapless_4953_pure_n_enchaine_pas() {
    let (dsp, piste_48k) = flux_44k_et_piste_48k();
    let mut e = etage(&dsp, Vec::new(), 44_100, 2, 16, 44_100, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));

    assert_eq!(
        e.enchainer_la_piste(&mut puits, piste_48k, PURE_SEUL, "DENAFRIPS"),
        Err(MotifDeReouverture::Pure),
        "PURE allumé : la piste enchaînée a été convertie en silence (#4953)"
    );
    assert!(e.resampler.is_none() && !e.needs_resample);
}

/// Le comportement voulu hors bit-perfect ne bouge pas : la piste est
/// convertie vers la cadence ouverte, et l'enchaînement reste sans blanc.
#[test]
fn chemin_gapless_4953_sans_exigence_la_piste_est_convertie_comme_avant() {
    let (dsp, piste_48k) = flux_44k_et_piste_48k();
    let mut e = etage(&dsp, Vec::new(), 44_100, 2, 16, 44_100, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));

    let verdict = e.enchainer_la_piste(
        &mut puits,
        piste_48k,
        ReglesDeCadence::default(),
        "Sortie partagée",
    );

    assert_eq!(
        verdict,
        Ok(true),
        "format changé : le convolveur est à refaire"
    );
    assert!(e.needs_resample && e.resampler.is_some());
    assert_eq!(e.sample_rate(), 48_000);
}

// ── 3. Le branchement ──────────────────────────────────────────────────────

#[test]
fn branchement_4953_la_boucle_gapless_porte_pure_et_la_preuve_de_l_ouverture() {
    let src = compact(include_str!("../local.rs"));
    assert!(
        src.contains("letperipherique_suit_la_source=backend.suit_la_cadence_source();"),
        "play_url doit relever, à l'ouverture, si le périphérique suit la source"
    );
    let regles = src
        .find("letregles=ReglesDeCadence{")
        .expect("la boucle gapless construit les règles de la frontière");
    let bloc = &src[regles..regles + 200];
    assert!(
        bloc.contains("pure:pure_bypass.load(Ordering::Relaxed),")
            && bloc.contains("peripherique_suit_la_source,}"),
        "la boucle gapless doit passer PURE (lu en vol) et la preuve de l'ouverture : {bloc}"
    );
    assert!(
        src[regles..].starts_with(
            "letregles=ReglesDeCadence{strict:strict_bitperfect,pure:pure_bypass.load(Ordering::Relaxed),peripherique_suit_la_source,};letOk(convolver_format_changed)=etage.enchainer_la_piste(&mut*puits,nouvelle_spec,regles,&device_name)else{break;};"
        ),
        "ces règles-là doivent aller à la frontière, et un refus doit sortir de la boucle"
    );
    // Aucune autre copie de la conversion gapless ne subsiste dans local.rs :
    // la ligne du relevé n'est émise qu'à un seul endroit, la frontière.
    assert_eq!(
        src.matches("\"local_audio_gapless_resampler_recreated\"")
            .count(),
        1,
        "une seconde copie de la conversion gapless contournerait la décision"
    );
}

#[test]
fn branchement_4953_l_ouverture_cpal_retient_la_mesure_et_la_cadence_ouverte() {
    let src = compact(include_str!("backend.rs"));
    assert!(src.contains("cadences_mesurees=rate_evidence.is_measured();"));
    assert!(
        src.contains(
            "suit_la_cadence_source:suit_la_cadence_source(cadences_mesurees,sample_rate,actual_config.sample_rate,),"
        ),
        "le backend doit juger sur la cadence RÉELLEMENT ouverte, après replis"
    );
}
