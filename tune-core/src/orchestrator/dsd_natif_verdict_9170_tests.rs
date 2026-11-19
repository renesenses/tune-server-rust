//! #9170 (fil 2951, Eversolo DAC-Z8 sous Linux) — le verdict « cette sortie
//! n'accepte pas le DSD natif » ne se pose que sur une preuve du pilote, et se
//! corrige.
//!
//! Avant : le premier sondage qui ne trouvait pas d'accès `hw:` direct (DAC
//! pas encore énuméré, repli sur le périphérique par défaut, périphérique
//! introuvable) retenait `SortieNonAsio` pour toute la vie du processus.
//! Toutes les pistes DSD « Natif » partaient ensuite en DoP (ou en PCM, un
//! DSD512 dépassant les 768 kHz du DAC), et les réglages affichaient « envoyé
//! en DoP ».

use super::sortie_alsa_apres_sonde;
use crate::orchestrator::{MotifDsdServiEnDop, SortieDsdLocale, decider_dsd_natif_local_voies};
use crate::outputs::capacite_dsd_natif::{CapaciteConnue, a_resonder, connue, retenir};
use crate::outputs::local::ResultatSondeDsdAlsa;

const DSD64: u32 = 2_822_400;
const DSD512: u32 = 22_579_200;

/// Les cadences DSD que le Z8 offre en `DSD_U32` (DSD64 → DSD512).
fn cadences_z8() -> Vec<u32> {
    vec![DSD64, 5_644_800, 11_289_600, DSD512]
}

/// Ce que l'orchestrateur fait d'un sondage : il le retient
/// (`SondeDsdAlsa::sonder_et_retenir`) puis décide la piste.
fn sonder(nom: &str, r: ResultatSondeDsdAlsa) -> SortieDsdLocale {
    r.retenir(nom);
    sortie_alsa_apres_sonde(r)
}

fn decider_dsd512(sortie: &SortieDsdLocale) -> Result<(), MotifDsdServiEnDop> {
    decider_dsd_natif_local_voies(sortie, Some(DSD512), Some(2), Some(768_000), false)
}

/// Le cas du fil 2951 : un premier sondage ne trouve pas le DAC (ou retombe
/// sur le périphérique par défaut), le suivant trouve son `hw:` — natif.
#[test]
fn sondage_echoue_puis_reussi_donne_du_natif() {
    let nom = "9170 Eversolo introuvable puis trouve";
    let sortie = sonder(nom, ResultatSondeDsdAlsa::Introuvable);
    assert_eq!(sortie, SortieDsdLocale::AlsaNonSondee);
    assert_eq!(
        connue(nom),
        None,
        "un périphérique introuvable ne prouve rien du DAC : rien n'est retenu"
    );
    assert!(a_resonder(nom), "la piste suivante doit re-sonder");

    let sortie = sonder(
        nom,
        ResultatSondeDsdAlsa::Sonde {
            cadences: cadences_z8(),
            voies: 2,
        },
    );
    assert_eq!(decider_dsd512(&sortie), Ok(()), "DSD512 natif attendu");
    assert_eq!(
        connue(nom),
        Some(CapaciteConnue::AlsaDsdU32 {
            cadences: cadences_z8()
        })
    );
    assert!(!a_resonder(nom), "une réponse du pilote est définitive");
}

/// Un nom résolu vers un PCM logiciel (le `hw:` n'était pas énuméré à cet
/// instant) : DoP pour cette piste, mais le verdict n'est que provisoire.
#[test]
fn un_pcm_logiciel_n_est_qu_un_verdict_provisoire() {
    let nom = "9170 Eversolo PCM logiciel puis hw";
    let sortie = sonder(nom, ResultatSondeDsdAlsa::PcmLogiciel);
    assert_eq!(sortie, SortieDsdLocale::NonAsio);
    assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
    assert!(
        a_resonder(nom),
        "un PCM logiciel observé sans réponse du pilote doit être re-sondé"
    );

    let sortie = sonder(
        nom,
        ResultatSondeDsdAlsa::Sonde {
            cadences: cadences_z8(),
            voies: 2,
        },
    );
    assert_eq!(decider_dsd512(&sortie), Ok(()));
    assert!(crate::outputs::capacite_dsd_natif::natif_annonce(Some(
        &format!("local:{nom}")
    )));
}

/// Contre-épreuve : le pilote ouvert répond SANS `DSD_U32` — c'est un vrai
/// refus, la piste part en DoP, et le verdict tient.
#[test]
fn un_refus_reel_du_pilote_donne_du_dop() {
    let nom = "9170 DAC sans DSD_U32";
    let sortie = sonder(
        nom,
        ResultatSondeDsdAlsa::Sonde {
            cadences: vec![],
            voies: 2,
        },
    );
    assert_eq!(
        decider_dsd512(&sortie),
        Err(MotifDsdServiEnDop::PiloteSansDsd)
    );
    assert_eq!(
        connue(nom),
        Some(CapaciteConnue::AlsaDsdU32 { cadences: vec![] })
    );
    assert!(!a_resonder(nom));
}

/// Un hôte audio qui n'est pas ALSA : structurel, définitif.
#[test]
fn un_hote_non_alsa_reste_definitif() {
    let nom = "9170 hote non ALSA";
    let sortie = sonder(nom, ResultatSondeDsdAlsa::PasUnPcmMateriel);
    assert_eq!(
        decider_dsd512(&sortie),
        Err(MotifDsdServiEnDop::SortieNonAsio)
    );
    assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
    assert!(!a_resonder(nom));
}

/// Un DAC prouvé natif, débranché ou occupé au sondage suivant, ou dont le
/// nom résout un instant vers un PCM logiciel : sa capacité reste.
#[test]
fn un_echec_ulterieur_n_efface_pas_une_capacite_prouvee() {
    let nom = "9170 DAC prouve puis debranche";
    retenir(
        nom,
        CapaciteConnue::AlsaDsdU32 {
            cadences: cadences_z8(),
        },
    );
    for r in [
        ResultatSondeDsdAlsa::Introuvable,
        ResultatSondeDsdAlsa::Occupe,
        ResultatSondeDsdAlsa::PcmLogiciel,
    ] {
        r.retenir(nom);
        assert_eq!(
            connue(nom),
            Some(CapaciteConnue::AlsaDsdU32 {
                cadences: cadences_z8()
            }),
            "{r:?} ne doit pas effacer une capacité prouvée"
        );
    }
}

/// Une lecture native réussie corrige un verdict « pas de natif » : le bras
/// ALSA confirme la capacité dès que le flux `DSD_U32` démarre. Lu dans la
/// source, faute de DAC sur la machine de test.
#[test]
fn le_bras_alsa_confirme_le_natif_quand_le_flux_demarre() {
    let src = std::fs::read_to_string("src/outputs/local/bras_alsa_dsd.rs")
        .expect("bras_alsa_dsd.rs lisible depuis la racine du crate");
    let demarre = src
        .find("\"local_audio_alsa_dsd_natif_playing\"")
        .expect("l'événement de démarrage du DSD natif a disparu");
    let confirme = src
        .find("capacite_dsd_natif::confirmer_natif_alsa(")
        .expect("le bras ne confirme plus la capacité après une lecture native");
    assert!(
        confirme > demarre,
        "la confirmation doit suivre le démarrage réel du flux, pas le précéder"
    );

    // Et la règle elle-même : la confirmation l'emporte sur tout verdict.
    let nom = "9170 confirme par la lecture";
    retenir(nom, CapaciteConnue::SortieNonAsio);
    crate::outputs::capacite_dsd_natif::confirmer_natif_alsa(nom, cadences_z8());
    assert!(crate::outputs::capacite_dsd_natif::natif_annonce(Some(
        &format!("local:{nom}")
    )));
}

/// Le sondage de la sortie passe par la règle de rétention ci-dessus, et
/// l'orchestrateur re-sonde une capacité provisoire au lieu de la croire.
#[test]
fn sondage_et_orchestrateur_passent_par_la_regle() {
    let local = std::fs::read_to_string("src/outputs/local.rs").unwrap();
    assert!(local.contains("r.retenir(&self.device_name);"));
    assert!(local.contains("capacite_dsd_natif::a_resonder_a_l_ouverture(&self.device_name)"));
    let resolve = std::fs::read_to_string("src/orchestrator/resolve_local.rs").unwrap();
    assert!(resolve.contains("if !capacite::a_resonder(&nom) {"));
    assert!(resolve.contains("return sortie_alsa_apres_sonde(r);"));
}
