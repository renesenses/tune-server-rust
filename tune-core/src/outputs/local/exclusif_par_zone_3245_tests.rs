//! #3245 — le mode exclusif d'une sortie déjà enregistrée change au repos,
//! jamais sous un flux ouvert.

use super::*;

#[test]
fn le_mode_exclusif_change_au_repos_3245() {
    let sortie = LocalOutput::with_options_and_endpoint("DAC".to_string(), None, false, "auto");
    assert!(!sortie.mode_exclusif());
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(true),
        ChangementDExclusif::Applique
    );
    assert!(
        sortie.mode_exclusif(),
        "la prochaine ouverture doit suivre le réglage de la zone"
    );
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(true),
        ChangementDExclusif::Inchange
    );
}

#[test]
fn le_mode_exclusif_ne_change_pas_sous_un_flux_ouvert_3245() {
    let sortie = LocalOutput::with_options_and_endpoint("DAC".to_string(), None, true, "auto");
    // Un fil de lecture vivant tient le périphérique.
    *sortie.sentinelle_du_fil.lock().unwrap() = Some(Arc::new(AtomicBool::new(true)));
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(false),
        ChangementDExclusif::DiffereEnLecture
    );
    assert!(
        sortie.mode_exclusif(),
        "rien ne doit changer pendant la lecture"
    );
    // Le fil rend le périphérique : le changement passe.
    *sortie.sentinelle_du_fil.lock().unwrap() = Some(Arc::new(AtomicBool::new(false)));
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(false),
        ChangementDExclusif::Applique
    );
    assert!(!sortie.mode_exclusif());
}

#[test]
fn la_regle_du_changement_d_exclusif_3245() {
    use ChangementDExclusif::*;
    assert_eq!(decider_le_changement_d_exclusif(true, true, true), Inchange);
    assert_eq!(
        decider_le_changement_d_exclusif(false, false, false),
        Inchange
    );
    assert_eq!(
        decider_le_changement_d_exclusif(false, true, true),
        DiffereEnLecture
    );
    assert_eq!(
        decider_le_changement_d_exclusif(true, false, false),
        Applique
    );
}

#[test]
fn le_mode_exclusif_ne_change_pas_pendant_la_lecture_3245() {
    let sortie = LocalOutput::with_options_and_endpoint("DAC".to_string(), None, false, "auto");
    sortie.playing.store(true, Ordering::SeqCst);
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(true),
        ChangementDExclusif::DiffereEnLecture
    );
    assert!(!sortie.mode_exclusif());
    sortie.playing.store(false, Ordering::SeqCst);
    assert_eq!(
        sortie.changer_de_mode_exclusif_au_repos(true),
        ChangementDExclusif::Applique
    );
}
