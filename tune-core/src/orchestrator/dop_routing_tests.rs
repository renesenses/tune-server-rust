use super::{
    AudioFormat, conteneur_a_profondeur_cachee, dop_requested, dop_wire_params,
    profondeur_sondee_si_la_base_ignore,
};

/// #1772 — le cas RÉEL de Marco Polo : Wiim Pro (renderer DLNA) relié en
/// optique à un DAC Denafrips, zone réglée sur « dop ». Avant le correctif,
/// ce choix n'était comparé nulle part pour une sortie réseau : le DAC
/// recevait du PCM 176,4 kHz, soit très exactement le débit DoP du DSD64 —
/// d'où un symptôme qui ressemblait à s'y méprendre à du DoP qui marche.
#[test]
fn un_renderer_reseau_regle_sur_dop_recoit_du_dop() {
    assert!(
        dop_requested(false, true, "dop"),
        "le choix explicite « dop » doit être honoré sur un renderer réseau"
    );
}

/// La sortie locale ne régresse pas : ses deux modes historiques passent
/// toujours par le DoP, faute pour une carte son de recevoir du DSD.
#[test]
fn la_sortie_locale_gardes_ses_deux_modes() {
    assert!(dop_requested(true, false, "native"));
    assert!(dop_requested(true, false, "dop"));
}

/// Le renderer réseau en « natif » ou « auto » ne doit PAS être détourné
/// vers le DoP : c'est `should_dsd_passthrough` qui arbitre, et lui seul
/// sait si l'appareil annonce le DSF/DFF.
#[test]
fn un_renderer_en_natif_ou_auto_n_est_pas_detourne() {
    assert!(!dop_requested(false, true, "native"));
    assert!(!dop_requested(false, true, "auto"));
    assert!(!dop_requested(false, true, ""));
}

/// « pcm » est un refus explicite : il ne produit jamais de DoP, nulle part.
#[test]
fn le_mode_pcm_ne_produit_jamais_de_dop() {
    assert!(!dop_requested(true, false, "pcm"));
    assert!(!dop_requested(false, true, "pcm"));
}

/// Une zone qui n'est ni locale ni réseau (navigateur, OAAT) ne reçoit pas
/// de DoP : ces chemins ont leur propre traitement du DSD.
#[test]
fn ni_locale_ni_reseau_ne_recoit_rien() {
    assert!(!dop_requested(false, false, "dop"));
    assert!(!dop_requested(false, false, "native"));
}

/// #1657 — le mode par DÉFAUT ne produit de DoP nulle part.
///
/// Ce test ne demande pas que « auto » change de comportement : il fixe le
/// fait, pour que personne ne le redécouvre en cherchant un défaut de
/// lecture. C'est ce fait, tu et non documenté, qui a fait passer un réglage
/// par défaut pour un DSD cassé.
#[test]
fn le_mode_auto_ne_produit_de_dop_nulle_part() {
    assert!(!dop_requested(true, false, "auto"));
    assert!(!dop_requested(false, true, "auto"));
    assert!(!dop_requested(false, false, "auto"));
    // Et le voisin qui piège tout autant : en RÉSEAU, « natif » non plus.
    assert!(!dop_requested(false, true, "native"));
}

/// #1654 — seul l'ALAC cache sa profondeur ; sonder le reste serait de
/// l'E/S pour rien.
#[test]
fn seul_lalac_a_une_profondeur_cachee() {
    assert!(conteneur_a_profondeur_cachee(Some(AudioFormat::Alac)));
    for f in [
        AudioFormat::Flac,
        AudioFormat::Wav,
        AudioFormat::Dsd,
        AudioFormat::Aac,
        AudioFormat::Mp3,
    ] {
        assert!(!conteneur_a_profondeur_cachee(Some(f)), "{f:?}");
    }
    assert!(!conteneur_a_profondeur_cachee(None));
}

/// Un fichier illisible ne doit jamais faire échouer la lecture : la sonde
/// rend `None`, l'appelant garde ce que dit la base.
#[test]
fn une_sonde_qui_echoue_laisse_la_base_decider() {
    assert_eq!(
        profondeur_sondee_si_la_base_ignore("/inexistant/x.m4a", Some(AudioFormat::Alac)),
        None
    );
    // Et un conteneur hors périmètre n'est même pas ouvert.
    assert_eq!(
        profondeur_sondee_si_la_base_ignore("/inexistant/x.flac", Some(AudioFormat::Flac)),
        None
    );
}

/// #1894 — l'en-tête WAV doit décrire le FICHIER, jamais la ligne `tracks`.
#[test]
fn le_fichier_prime_sur_la_base_pour_annoncer_un_flux_dop() {
    // Le cas qui produit du bruit blanc : la base dit stéréo, le fichier
    // est multicanal. Annoncer 2 canaux pour une charge utile qui en porte
    // 5 décale chaque mot de 24 bits et noie le marqueur DoP.
    let (rate, ch) = dop_wire_params(Some((2_822_400, 5)), Some(2_822_400), 2);
    assert_eq!(ch, 5);
    assert_eq!(rate, 2_822_400);

    // Et le cas symétrique : une cadence périmée en base (DSD64 scanné,
    // fichier remplacé par du DSD128) annoncerait un débit DoP faux.
    let (rate, ch) = dop_wire_params(Some((5_644_800, 2)), Some(2_822_400), 2);
    assert_eq!(rate, 5_644_800);
    assert_eq!(ch, 2);
}

#[test]
fn un_entete_dsd_illisible_retombe_sur_la_base_plutot_que_de_refuser() {
    // Mieux vaut diffuser avec des valeurs approximatives que ne rien lire.
    let (rate, ch) = dop_wire_params(None, Some(5_644_800), 2);
    assert_eq!(rate, 5_644_800);
    assert_eq!(ch, 2);

    // Base muette : le défaut DSD64, et jamais moins de deux canaux —
    // un en-tête WAV à 0 canal est injouable partout.
    let (rate, ch) = dop_wire_params(None, None, 0);
    assert_eq!(rate, 2_822_400);
    assert_eq!(ch, 2);
}

#[test]
fn le_plancher_a_deux_canaux_ne_masque_jamais_le_fichier() {
    // `.max(2)` est un plancher, pas un plafond : il ne doit pas rabattre
    // un fichier multicanal — c'était le risque du `track.channels.max(2)`
    // d'origine, qui ignorait le fichier de bout en bout.
    assert_eq!(dop_wire_params(Some((2_822_400, 6)), None, 2).1, 6);
    assert_eq!(dop_wire_params(Some((2_822_400, 1)), None, 2).1, 2);
}
