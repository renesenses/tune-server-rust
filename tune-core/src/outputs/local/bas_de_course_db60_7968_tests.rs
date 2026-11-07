//! #7968 — le bas de la course `db60` d'une sortie locale : chaque position
//! du curseur donne un niveau distinct, et seule la position 0 est muette.
//!
//! `set_volume` tronquait le gain en millièmes (`as u32`), puis
//! `effective_volume_units` arrondissait de nouveau le produit au millième :
//! 1 % du curseur (gain 0,0002) valait 0, le silence courait jusqu'à 5 %, et
//! au-dessus quatre ou cinq positions voisines tombaient sur le même
//! millième. Le gain est désormais rangé en
//! [`UNITE_DU_GAIN_DE_RENDU`] (millionièmes) jusqu'au rendu.

use std::sync::atomic::Ordering;

use super::LocalOutput;
use super::gain_au_rendu_7264::GainAuRendu;
use crate::audio::volume_scale::{UNITE_DU_GAIN_DE_RENDU, gain_perceptif};
use crate::outputs::traits::OutputTarget;

/// Le gain que les rappels de rendu multiplient quand le curseur `db60` est
/// à `position` %, posé par le vrai `set_volume`.
async fn gain_rendu_a(sortie: &LocalOutput, position: u32) -> u32 {
    sortie
        .set_volume(gain_perceptif(f64::from(position) / 100.0))
        .await
        .expect("set_volume");
    sortie.gain_de_rendu().load(Ordering::SeqCst)
}

/// LE témoin de l'issue : positions 1 à 10 % sur `db60`, gains strictement
/// croissants, aucun nul hors 0, et chacun fidèle à la loi à 1 % près.
#[tokio::test]
async fn positions_1_a_10_du_curseur_db60_donnent_des_gains_strictement_croissants_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    assert_eq!(gain_rendu_a(&sortie, 0).await, 0, "0 % : le silence exact");

    let mut precedent = 0;
    for position in 1..=10 {
        let rendu = gain_rendu_a(&sortie, position).await;
        assert!(
            rendu > 0,
            "{position} % du curseur db60 rendu muet : le gain a été arrondi à 0"
        );
        assert!(
            rendu > precedent,
            "{position} % du curseur db60 rend {rendu}, pas plus que {} % ({precedent}) : \
             deux positions au même niveau",
            position - 1
        );
        let attendu = gain_perceptif(f64::from(position) / 100.0);
        let lu = f64::from(rendu) / f64::from(UNITE_DU_GAIN_DE_RENDU);
        assert!(
            ((lu - attendu) / attendu).abs() < 0.01,
            "{position} % du curseur db60 : gain rendu {lu}, la loi demande {attendu}"
        );
        precedent = rendu;
    }
}

/// Le gain réellement appliqué au mot par le rendu natif (#7264) suit : à
/// 1 % du curseur, `GainAuRendu` multiplie par ≈ 0,0002, pas par 0. Et une
/// atténuation ReplayGain de −6 dB par-dessus ne rend pas la sortie muette.
#[tokio::test]
async fn a_un_pour_cent_le_rendu_natif_applique_un_gain_non_nul_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    gain_rendu_a(&sortie, 1).await;
    let gain = GainAuRendu::nouveau(sortie.gain_de_rendu(), 48_000, 2);
    let attendu = gain_perceptif(0.01);
    assert!(
        ((gain.courant() - attendu) / attendu).abs() < 0.01,
        "1 % du curseur db60 : le rendu applique {}, la loi demande {attendu}",
        gain.courant()
    );

    sortie.set_replaygain_factor(0.5);
    assert!(
        sortie.gain_de_rendu().load(Ordering::SeqCst) > 0,
        "1 % du curseur et ReplayGain −6 dB : sortie muette"
    );
}

/// La sourdine, puis son retrait, rendent le niveau exact d'avant : le bas
/// de course retrouvé n'est pas perdu par un aller-retour.
#[tokio::test]
async fn la_sourdine_rend_le_niveau_fin_d_avant_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    let avant = gain_rendu_a(&sortie, 3).await;
    sortie.set_mute(true).await.expect("set_mute");
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), 0);
    sortie.set_mute(false).await.expect("set_mute");
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), avant);
    assert!(avant > 0, "3 % du curseur db60 rendu muet");
}
