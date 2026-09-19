//! #4177 — une sortie exclusive Windows REND son périphérique à la pause ; la
//! reprise doit alors rétablir la lecture à sa position, pas reprendre « sur
//! place » un flux qui n'existe plus.
//!
//! Avant : `pause()` de la sortie locale posait un booléen, le fil de rendu
//! WASAPI exclusif poussait du silence et le point de sortie restait pris —
//! « quand on met Tune en pause, il n'est pas possible d'écouter un clip sur
//! YouTube » (Jean Valjean, fil 1798). La sortie rend désormais le
//! périphérique et le dit (`device_released_on_pause`) ; ici, la matrice de
//! reprise tient compte de cette réponse.

use super::{RepriseDeSession, reprise_de_session};

/// Le témoin : session vivante, pause courte — sans le drapeau, c'était
/// `SurPlace` (et un `resume()` qui ne rouvrait rien). Avec, on rétablit à la
/// position.
#[test]
fn un_peripherique_rendu_a_la_pause_retablit_la_piste_a_sa_position_4177() {
    assert_eq!(
        reprise_de_session(false, true, false, false, true),
        RepriseDeSession::RetablirALaPosition
    );
    assert_eq!(
        reprise_de_session(false, true, false, false, false),
        RepriseDeSession::SurPlace,
        "sans périphérique rendu, la table d'avant est intacte"
    );
}

/// Une radio dont le périphérique a été rendu repart en DIRECT, comme quand
/// son producteur est mort ; sans URL de station, rien à rejouer.
#[test]
fn un_peripherique_rendu_a_la_pause_rejoue_le_direct_radio_4177() {
    assert_eq!(
        reprise_de_session(true, true, false, false, true),
        RepriseDeSession::RejouerLeDirect
    );
    assert_eq!(
        reprise_de_session(true, false, false, false, true),
        RepriseDeSession::SurPlace
    );
}

/// Périphérique rendu et piste non identifiable : on le DIT, on ne se tait pas.
#[test]
fn un_peripherique_rendu_sans_piste_identifiable_s_explique_4177() {
    assert_eq!(
        reprise_de_session(false, false, false, false, true),
        RepriseDeSession::Expliquer
    );
}

/// La garde du BRANCHEMENT : `resume` interroge la sortie AVANT la décision,
/// et la réponse entre dans `reprise_de_session`.
#[test]
fn resume_demande_a_la_sortie_si_elle_a_rendu_le_peripherique_4177() {
    let src = include_str!("transport.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let question = prod
        .find("device_released_on_pause()")
        .expect("resume doit interroger la sortie");
    let decision = prod
        .find("let decision = reprise_de_session(")
        .expect("la décision de reprise");
    assert!(question < decision, "la question doit précéder la décision");
    assert!(
        prod[decision..decision + 300].contains("peripherique_rendu,"),
        "la réponse doit entrer dans la décision"
    );
}
