// ── Garde n°2 : l'etat du transport ne commande pas les metadonnees ────
//
// Le rafraichissement vivait dans le `if !radio_stopped`, aux cotes de la
// synchro de volume — qui, elle, a bien besoin d'un renderer en lecture.
// Un renderer qui ne demarrait pas figeait donc l'affichage sur le nom de
// la station, et un bug de LECTURE se deguisait en bug de METADONNEES.

#[test]
fn seul_le_temps_ecoule_commande_le_sondage_radio() {
    use super::decisions::radio_poll_due;
    use std::time::Duration;
    assert!(!radio_poll_due(Duration::from_secs(3), 15));
    assert!(radio_poll_due(Duration::from_secs(15), 15));
    assert!(radio_poll_due(Duration::from_secs(600), 15));
}

#[test]
fn les_deux_chemins_partagent_la_meme_cadence() {
    // La zone SANS peripherique (#1536) et la zone AVEC doivent sonder au
    // meme rythme : une seule regle de temps, pas deux qui derivent.
    use super::decisions::{deviceless_radio_refresh_due, radio_poll_due};
    use std::time::Duration;
    for secs in [0_u64, 5, 14, 15, 60] {
        let d = Duration::from_secs(secs);
        assert_eq!(
            deviceless_radio_refresh_due(true, Some("radio"), d, 15),
            radio_poll_due(d, 15),
            "cadence divergente a {secs}s"
        );
    }
}

// ── Metadonnees radio sur une zone SANS peripherique (fil « Metadonnees
//    radio disparues ? ») ──────────────────────────────────────────────
//
// Le poller quittait toute zone sans peripherique avant d'arriver au
// rafraichissement des metadonnees : sur « Cet ordinateur », l'appel
// n'existait pas. Deux testeurs, meme station, meme version, resultats
// opposes — l'un sur une vraie sortie, l'autre sur le navigateur.

use super::decisions::deviceless_radio_refresh_due as due;
use std::time::Duration;

#[test]
fn zone_navigateur_qui_joue_une_radio_est_rafraichie() {
    assert!(due(true, Some("radio"), Duration::from_secs(20), 15));
}

#[test]
fn letranglement_est_respecte() {
    // Le tick est a la seconde, l'API de la station ne doit pas l'etre.
    assert!(!due(true, Some("radio"), Duration::from_secs(3), 15));
    // Pile a l'echeance : on y va.
    assert!(due(true, Some("radio"), Duration::from_secs(15), 15));
}

#[test]
fn un_fichier_local_ne_declenche_aucun_appel_reseau() {
    // Une zone navigateur qui joue un fichier passe ici a chaque tick.
    assert!(!due(true, Some("local"), Duration::from_secs(600), 15));
    assert!(!due(true, None, Duration::from_secs(600), 15));
}

#[test]
fn zone_a_larret_nest_pas_sondee() {
    assert!(!due(false, Some("radio"), Duration::from_secs(600), 15));
}
