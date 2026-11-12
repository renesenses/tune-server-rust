//! #8248 — la règle du DSD natif local sur une sortie ALSA `hw:` : natif
//! quand le pilote offre `DSD_U32` à la cadence et aux voies du fichier, DoP
//! sinon, avec le motif qui le dit. Le Sabaj D5 d'Erik (fil 2730) sert de
//! témoin : `DSD_U32_BE` de 88,2 à 768 kHz, soit DSD64 → DSD512, pas DSD1024.

use super::{
    MotifDsdServiEnDop, SortieDsdLocale, TransportDsd, decider_dsd_natif_local_voies,
    transport_dsd, transport_dsd_publie,
};
use crate::audio::dsd_u32_alsa::cadences_offertes;

const DSD64: u32 = 2_822_400;
const DSD512: u32 = 22_579_200;
const DSD512_48: u32 = 24_576_000;
const DSD1024: u32 = 45_158_400;

fn sabaj_d5() -> SortieDsdLocale {
    let trames = [
        88_200u32, 176_400, 352_800, 705_600, 768_000, 96_000, 192_000, 384_000,
    ];
    SortieDsdLocale::Alsa {
        cadences: cadences_offertes(|r| trames.contains(&r)),
        voies: 2,
    }
}

/// Le constat d'Erik : une zone « Natif » sur un PCM `hw:` qui offre
/// `DSD_U32_BE` part en DSD natif, de DSD64 à DSD512 (deux familles).
#[test]
fn un_pcm_qui_offre_dsd_u32_donne_du_natif() {
    for cadence in [DSD64, DSD512, DSD512_48] {
        for strict in [false, true] {
            assert_eq!(
                decider_dsd_natif_local_voies(&sabaj_d5(), Some(cadence), Some(2), None, strict),
                Ok(()),
                "DSD {cadence} Hz offert par le pilote : natif attendu"
            );
        }
    }
}

/// Le repli DoP est INCHANGÉ quand le format n'est pas offert, et il le dit.
#[test]
fn sans_dsd_u32_le_dop_reste_et_le_motif_le_dit() {
    let sans = SortieDsdLocale::Alsa {
        cadences: vec![],
        voies: 2,
    };
    assert_eq!(
        decider_dsd_natif_local_voies(&sans, Some(DSD64), Some(2), None, false),
        Err(MotifDsdServiEnDop::PiloteSansDsd)
    );
    assert_eq!(
        decider_dsd_natif_local_voies(&sabaj_d5(), Some(DSD1024), Some(2), None, false),
        Err(MotifDsdServiEnDop::CadenceNonDeclaree),
        "DSD1024 non offert par le Sabaj : jamais ouvert en natif"
    );
    assert_eq!(
        decider_dsd_natif_local_voies(
            &SortieDsdLocale::AlsaNonSondee,
            Some(DSD64),
            Some(2),
            None,
            false
        ),
        Err(MotifDsdServiEnDop::CapaciteNonSondee),
        "PCM tenu : on ne force pas le pilote"
    );
    assert_eq!(
        decider_dsd_natif_local_voies(&SortieDsdLocale::NonAsio, Some(DSD64), Some(2), None, false),
        Err(MotifDsdServiEnDop::SortieNonAsio),
        "plughw:/default/PipeWire : pas de DSD natif"
    );
}

/// Un fichier multicanal sur un pilote sondé en stéréo : DoP, motif dédié.
#[test]
fn les_voies_du_fichier_doivent_etre_celles_du_pilote() {
    assert_eq!(
        decider_dsd_natif_local_voies(&sabaj_d5(), Some(DSD64), Some(6), None, false),
        Err(MotifDsdServiEnDop::VoiesNonDeclarees)
    );
    assert_eq!(
        decider_dsd_natif_local_voies(&sabaj_d5(), Some(DSD64), None, None, false),
        Err(MotifDsdServiEnDop::VoiesNonDeclarees)
    );
    assert_eq!(
        MotifDsdServiEnDop::VoiesNonDeclarees.as_str(),
        "voies_non_declarees_par_le_pilote"
    );
}

/// Le plafond de cadence de la zone s'applique comme au DoP (cadence / 16).
#[test]
fn le_plafond_de_zone_vaut_aussi_pour_le_natif_alsa() {
    assert_eq!(
        decider_dsd_natif_local_voies(&sabaj_d5(), Some(DSD512), Some(2), Some(705_600), false),
        Err(MotifDsdServiEnDop::PlafondDeZone)
    );
    assert_eq!(
        decider_dsd_natif_local_voies(&sabaj_d5(), Some(DSD512), Some(2), Some(1_411_200), false),
        Ok(())
    );
}

/// Les réglages de zone restent maîtres : seul « Natif » peut mener au
/// natif ; « DoP » reste du DoP, « Auto » du PCM, quel que soit le pilote.
#[test]
fn le_reglage_de_zone_reste_maitre() {
    assert_eq!(transport_dsd(true, false, "dop"), TransportDsd::Dop);
    assert_eq!(transport_dsd(true, false, "auto"), TransportDsd::Pcm);
    assert_eq!(
        transport_dsd(true, false, "native"),
        TransportDsd::NatifServiEnDop
    );
    assert_eq!(
        transport_dsd_publie(true, false, "dop", true),
        TransportDsd::Dop
    );
}

/// La table de capacité : un PCM ALSA sondé avec des cadences annonce le
/// natif à l'API ; une cadence refusée à l'ouverture en est retirée.
#[test]
fn la_table_retient_la_capacite_alsa() {
    use crate::outputs::capacite_dsd_natif::{
        CapaciteConnue, natif_annonce, oublier_cadence, retenir,
    };
    let nom = "Sabaj D5 8248-test";
    let id = format!("local:{nom}");
    retenir(
        nom,
        CapaciteConnue::AlsaDsdU32 {
            cadences: vec![DSD64],
        },
    );
    assert!(natif_annonce(Some(&id)));
    oublier_cadence(nom, DSD64);
    assert!(
        !natif_annonce(Some(&id)),
        "cadence refusée à l'ouverture : retirée"
    );
    retenir(nom, CapaciteConnue::AlsaDsdU32 { cadences: vec![] });
    assert!(
        !natif_annonce(Some(&id)),
        "pilote sans DSD_U32 : rien d'annoncé"
    );
}
