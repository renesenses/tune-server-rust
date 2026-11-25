//! #9171 — la jauge ReplayGain n'est plus bloquée à 100 % quand un scan
//! ajoute des pistes pendant la passe.
//!
//! Le total était figé à l'ouverture de la campagne, puis seulement aligné
//! sur le compteur (`traitees > total` → `total = traitees`) : la jauge
//! restait pleine pendant que le compteur montait. Le total épuisé est
//! désormais recompté au lot suivant (`progression::ouvrir_si_besoin`).
//!
//! Une cible `[[test]]` à elle seule : l'état de `progression` est global au
//! processus, et le témoin #4144 le remet à zéro de son côté.

use tune_core::audio::replaygain::progression;

#[test]
fn un_total_replaygain_epuise_est_recompte_au_lot_suivant_9171() {
    progression::reinitialiser_pour_les_essais();
    progression::ouvrir_si_besoin(|| 2);
    progression::avancer();
    progression::avancer();
    // Le scan a ajouté 3 pistes : le lot suivant rouvre la campagne.
    progression::ouvrir_si_besoin(|| 3);
    let a = progression::releve();
    assert!(a.actif);
    assert_eq!(
        (a.traitees, a.total),
        (2, 5),
        "le total épuisé doit être recompté"
    );
    // Total non épuisé : aucun nouveau comptage.
    progression::avancer();
    progression::ouvrir_si_besoin(|| panic!("ne recompte pas"));
    progression::avancer();
    progression::avancer();
    // Plus rien d'ajouté : le total reste.
    progression::ouvrir_si_besoin(|| 0);
    let a = progression::releve();
    assert_eq!((a.traitees, a.total), (5, 5));
    progression::au_repos();
    assert!(!progression::releve().actif);
}
