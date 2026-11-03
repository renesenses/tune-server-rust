//! #7340 — directives APO stéréo, sans ouvrir de sortie audio.
use tune_core::audio::autoeq::{ErreurAutoEq, analyser};

const FILTRE: &str = "Filter 1: ON PK Fc 100 Hz Gain -3 dB Q 1\n";

#[test]
fn gauche_droite_et_all_conservent_le_contexte() {
    let profil = analyser(&format!(
        "Preamp: -6 dB\nChannel: L\n{FILTRE}Channel: R\n{FILTRE}Channel: all\n{FILTRE}"
    ))
    .unwrap();
    assert_eq!(
        profil.bandes.iter().map(|b| b.channel).collect::<Vec<_>>(),
        vec![Some(0), Some(1), None]
    );
    assert_eq!(profil.preamp_db, -6.0);
}

#[test]
fn positions_stereo_et_selection_multiple_ne_dupliquent_pas_un_canal() {
    let profil = analyser(&format!("Channel : 2 L R 1 # stéréo\n{FILTRE}")).unwrap();
    assert_eq!(
        profil.bandes.iter().map(|b| b.channel).collect::<Vec<_>>(),
        vec![Some(1), Some(0)]
    );
    let global = analyser(&format!("Channel: ALL\n{FILTRE}")).unwrap();
    assert_eq!(global.bandes[0].channel, None);
}

#[test]
fn selection_inconnue_et_preamp_cible_refusent_tout_le_profil() {
    for selection in ["", "C", "0", "3", "LFE", "L R all", "L, R"] {
        let error = analyser(&format!("{FILTRE}Channel: {selection}\n{FILTRE}")).unwrap_err();
        assert!(
            matches!(error, ErreurAutoEq::CanauxInvalides { ligne: 2, .. }),
            "{selection}: {error}"
        );
    }
    let error = analyser(&format!("Channel: L\nPreamp: -4 dB\n{FILTRE}")).unwrap_err();
    assert!(matches!(error, ErreurAutoEq::PreampParCanal { ligne: 2 }));
}

#[test]
fn un_fichier_ancien_reste_global_et_un_off_reste_ignore() {
    let profil = analyser(&format!("{FILTRE}Channel: R\nFilter 2: OFF\n")).unwrap();
    assert_eq!(profil.bandes.len(), 1);
    assert_eq!(profil.bandes[0].channel, None);
    assert_eq!(profil.ignores[0].ligne, 3);
}

#[test]
fn la_marge_droite_ne_se_fait_pas_passer_pour_celle_de_gauche() {
    let profil =
        analyser("Preamp: -3 dB\nChannel: R\nFilter 1: ON PK Fc 100 Hz Gain 6 dB Q 1\n").unwrap();
    let marges = profil.marges_reservees_db();
    assert!(
        marges[0].abs() < 0.0001,
        "gauche sans filtre: {}",
        marges[0]
    );
    assert!(marges[1] < -3.0, "droite amplifiée: {}", marges[1]);
    assert!(profil.marge_reservee_db().abs() < 0.0001);
    assert!(!profil.marge_de_tune_couvre_le_preamp());
}
