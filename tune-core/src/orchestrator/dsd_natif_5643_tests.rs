//! #5643, lot E — la règle du DSD natif local, et ce que l'API en publie.
//!
//! `decider_dsd_natif_local` ne voit jamais un pilote : elle reçoit ce que
//! l'orchestrateur sait de la sortie (`SortieDsdLocale`) et rend `Ok` ou le
//! motif du repli DoP. Chaque axe de la mission y a son témoin : capacité
//! déclarée ou non, cadence, plafond de zone, mode strict, sortie non ASIO.

use super::{
    MotifDsdServiEnDop, SortieDsdLocale, TransportDsd, decider_dsd_natif_local, transport_dsd,
    transport_dsd_publie,
};

const DSD64: u32 = 2_822_400;
const DSD128: u32 = 5_644_800;
const DSD256: u32 = 11_289_600;

fn asio(cadences: &[u32]) -> SortieDsdLocale {
    SortieDsdLocale::Asio {
        cadences: cadences.to_vec(),
    }
}

/// Le témoin positif : pilote ASIO qui déclare DSD64 et DSD128, fichier
/// DSD128, pas de plafond → natif. Avec ou sans mode strict.
#[test]
fn capacite_declaree_a_la_cadence_du_fichier_donne_du_natif() {
    for strict in [false, true] {
        assert_eq!(
            decider_dsd_natif_local(&asio(&[DSD64, DSD128]), Some(DSD128), None, strict),
            Ok(()),
            "strict = {strict}"
        );
    }
}

/// Le pilote répond, mais ne déclare aucune cadence DSD (pilote PCM).
#[test]
fn pilote_sans_dsd_reste_en_dop() {
    assert_eq!(
        decider_dsd_natif_local(&asio(&[]), Some(DSD64), None, false),
        Err(MotifDsdServiEnDop::PiloteSansDsd)
    );
}

/// Le pilote déclare le DSD, mais pas à la cadence de CE fichier.
#[test]
fn cadence_non_declaree_reste_en_dop() {
    assert_eq!(
        decider_dsd_natif_local(&asio(&[DSD64, DSD128]), Some(DSD256), None, false),
        Err(MotifDsdServiEnDop::CadenceNonDeclaree)
    );
    // Une cadence qui n'est même pas une cadence DSD native.
    assert_eq!(
        decider_dsd_natif_local(&asio(&[DSD64]), Some(44_100), None, false),
        Err(MotifDsdServiEnDop::CadenceNonDeclaree)
    );
}

/// Pilote occupé par un autre flux : pas de sondage, donc pas de natif.
#[test]
fn capacite_non_sondee_reste_en_dop() {
    assert_eq!(
        decider_dsd_natif_local(&SortieDsdLocale::AsioNonSondee, Some(DSD64), None, false),
        Err(MotifDsdServiEnDop::CapaciteNonSondee)
    );
}

/// WASAPI, partagé, macOS, Linux/ALSA : jamais de natif, quoi qu'on sache.
#[test]
fn sortie_non_asio_reste_en_dop() {
    assert_eq!(
        decider_dsd_natif_local(&SortieDsdLocale::NonAsio, Some(DSD64), None, false),
        Err(MotifDsdServiEnDop::SortieNonAsio)
    );
}

/// En-tête illisible : la cadence est inconnue, on ne promet rien.
#[test]
fn cadence_inconnue_reste_en_dop() {
    assert_eq!(
        decider_dsd_natif_local(&asio(&[DSD64]), None, None, false),
        Err(MotifDsdServiEnDop::CadenceInconnue)
    );
}

/// Le plafond de zone s'applique comme au DoP : sur le débit équivalent
/// `cadence / 16`. DSD128 = 352,8 kHz : refusé sous 192 kHz, admis à 384 kHz
/// et à 352,8 kHz pile. Le mode strict ne change pas la décision (le repli
/// DoP appliquera son propre refus strict, #3973).
#[test]
fn le_plafond_de_zone_borne_le_natif_comme_le_dop() {
    for strict in [false, true] {
        assert_eq!(
            decider_dsd_natif_local(&asio(&[DSD64, DSD128]), Some(DSD128), Some(192_000), strict),
            Err(MotifDsdServiEnDop::PlafondDeZone),
            "strict = {strict}"
        );
        assert_eq!(
            decider_dsd_natif_local(&asio(&[DSD64, DSD128]), Some(DSD128), Some(384_000), strict),
            Ok(())
        );
        assert_eq!(
            decider_dsd_natif_local(&asio(&[DSD128]), Some(DSD128), Some(352_800), strict),
            Ok(())
        );
    }
}

/// `transport_dsd` seul ne mène JAMAIS au natif : seule la règle ci-dessus,
/// qui connaît le pilote, peut promouvoir `NatifServiEnDop`.
#[test]
fn transport_dsd_seul_ne_rend_jamais_natif() {
    for local in [false, true] {
        for reseau in [false, true] {
            for mode in ["native", "dop", "pcm", "auto", ""] {
                assert_ne!(transport_dsd(local, reseau, mode), TransportDsd::Natif);
            }
        }
    }
}

/// Le libellé publié, que le client web (#1879) lit : « natif ».
#[test]
fn le_libelle_natif_est_stable_et_tient_sa_promesse() {
    assert_eq!(TransportDsd::Natif.as_str(), "natif");
    assert!(TransportDsd::Natif.tient_sa_promesse());
    assert_eq!(TransportDsd::NatifServiEnDop.as_str(), "natif_servi_en_dop");
}

/// Ce que l'API publie : « natif » seulement pour une zone locale réglée
/// « natif » dont la sortie a DÉCLARÉ le DSD. Tout le reste est inchangé.
#[test]
fn l_api_publie_natif_seulement_quand_la_capacite_est_connue() {
    assert_eq!(
        transport_dsd_publie(true, false, "native", true),
        TransportDsd::Natif
    );
    assert_eq!(
        transport_dsd_publie(true, false, "native", false),
        TransportDsd::NatifServiEnDop
    );
    assert_eq!(
        transport_dsd_publie(true, false, "dop", true),
        TransportDsd::Dop,
        "« dop » demandé reste du DoP"
    );
    assert_eq!(
        transport_dsd_publie(false, true, "native", true),
        TransportDsd::Pcm,
        "réseau : rien ne change"
    );
    assert_eq!(
        transport_dsd_publie(true, false, "", true),
        TransportDsd::Pcm
    );
}

/// La table de capacité : « natif » annoncé pour `local:<nom>` dès qu'une
/// cadence est déclarée, plus du tout quand elle est retirée après un refus
/// d'ouverture, jamais pour une sortie non ASIO.
#[test]
fn la_table_de_capacite_suit_le_pilote() {
    use crate::outputs::capacite_dsd_natif::{
        CapaciteConnue, natif_annonce, oublier_cadence, retenir,
    };
    let nom = "SMSL USB DAC 5643-test";
    let id = format!("local:{nom}");
    assert!(!natif_annonce(Some(&id)), "inconnu : rien d'annoncé");
    retenir(
        nom,
        CapaciteConnue::Asio {
            cadences: vec![DSD64],
        },
    );
    assert!(natif_annonce(Some(&id)));
    assert!(!natif_annonce(Some(nom)), "sans le préfixe local:, rien");
    oublier_cadence(nom, DSD64);
    assert!(
        !natif_annonce(Some(&id)),
        "cadence refusée à l'ouverture : retirée"
    );
    retenir(nom, CapaciteConnue::SortieNonAsio);
    assert!(!natif_annonce(Some(&id)));
    assert!(!natif_annonce(None));
}
