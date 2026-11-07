//! #5410 / #6176 — la garde thermique des analyses hors Linux.
//!
//! Didier (fil 2239, Windows 11, Ryzen 7 3800X) : un PC arrêté en surchauffe
//! pendant le calcul de la plage dynamique. Sous Windows et macOS la
//! température n'était jamais lue : la garde « arrêt à 80 °C, reprise à
//! 70 °C » ne protégeait rien, et les analyses tournaient à pleine largeur.
//!
//! Ces témoins portent sur la DÉCISION, en logique pure, testable partout :
//! * sans température lisible, les analyses de fond gardent la vitesse
//!   réglée (#6450 : une température illisible n'est pas trop haute ; le
//!   bridage à un fichier de #5410 est retiré) ;
//! * une température lisible laisse la vitesse réglée intacte, et sa garde
//!   arrête toujours les analyses au-delà de 80 °C ;
//! * une zone thermique FIGÉE (même valeur pendant longtemps : la zone ACPI de
//!   beaucoup de cartes de bureau) ne vaut pas une mesure ;
//! * la conversion des compteurs Windows (dixièmes de kelvin, kelvins).
//!
//! La lecture Windows elle-même (PDH, WMI) ne se teste pas ici : le choix
//! entre ses sources est éprouvé avec des sources simulées
//! (`thermal_sources_6450_tests.rs`).

use std::time::{Duration, Instant};

use super::{
    LectureThermique, MotifIllisible, PAUSE_ABOVE_C, SuiviDeStabilite, ThermalGate, Verdict,
    celsius_depuis_dixiemes_de_kelvin, celsius_depuis_kelvins, largeur_selon_la_chaleur,
};

/// 🔴 #6450 — Windows 11, Ryzen 7 3800X : aucune zone thermique lisible, et
/// les analyses passaient à UN fichier à la fois pour toujours. Une
/// température illisible (aucun capteur ou zone figée) garde désormais la
/// vitesse réglée.
#[test]
fn une_temperature_illisible_ne_bride_plus_les_analyses_6450() {
    for motif in [MotifIllisible::AucunCapteur, MotifIllisible::Figee] {
        let illisible = LectureThermique::Illisible(motif);
        assert_eq!(
            largeur_selon_la_chaleur(4, &illisible),
            4,
            "{motif:?} : température illisible, la vitesse rapide (4) est bridée"
        );
        assert_eq!(
            largeur_selon_la_chaleur(2, &illisible),
            2,
            "{motif:?} : température illisible, la vitesse normale (2) est bridée"
        );
        assert_eq!(largeur_selon_la_chaleur(0, &illisible), 1, "jamais zéro");
    }
}

/// Une température LUE et trop haute arrête toujours les analyses : c'est la
/// garde chaude, pas la largeur, qui les retient, et #6450 n'y touche pas.
#[test]
fn une_temperature_lue_trop_haute_arrete_toujours_les_analyses_6450() {
    let chaude = LectureThermique::Lisible(85.0);
    assert_eq!(largeur_selon_la_chaleur(4, &chaude), 4);
    let mut garde = ThermalGate::new();
    assert!(
        matches!(
            garde.decide(chaude.celsius()),
            Verdict::Hold { entering: true, .. }
        ),
        "85 °C lus : la garde doit suspendre (seuil {PAUSE_ABOVE_C} °C)"
    );
    // Et une température illisible ne suspend rien.
    let mut garde = ThermalGate::new();
    assert!(matches!(
        garde.decide(LectureThermique::Illisible(MotifIllisible::AucunCapteur).celsius()),
        Verdict::Go { temp_c: None, .. }
    ));
}

#[test]
fn une_temperature_lisible_laisse_la_vitesse_reglee() {
    let lisible = LectureThermique::Lisible(55.0);
    assert_eq!(largeur_selon_la_chaleur(2, &lisible), 2);
    assert_eq!(largeur_selon_la_chaleur(4, &lisible), 4);
    assert_eq!(largeur_selon_la_chaleur(1, &lisible), 1);
    // Jamais zéro, même sur une entrée dégénérée.
    assert_eq!(largeur_selon_la_chaleur(0, &lisible), 1);
}

#[test]
fn aucune_valeur_est_illisible() {
    let mut s = SuiviDeStabilite::default();
    assert_eq!(
        s.observer(None, Instant::now()),
        LectureThermique::Illisible(MotifIllisible::AucunCapteur)
    );
}

#[test]
fn une_zone_qui_ne_bouge_jamais_est_declaree_figee() {
    let mut s = SuiviDeStabilite::default();
    let t0 = Instant::now();
    // 27,85 °C (301 K) pendant onze minutes, une lecture toutes les 30 s :
    // la valeur que rendent beaucoup de zones ACPI de cartes de bureau.
    let mut derniere = LectureThermique::Lisible(0.0);
    for i in 0..23u64 {
        derniere = s.observer(Some(27.85), t0 + Duration::from_secs(30 * i));
    }
    assert_eq!(derniere, LectureThermique::Illisible(MotifIllisible::Figee));
}

#[test]
fn avant_d_avoir_assez_observe_la_zone_est_crue() {
    let mut s = SuiviDeStabilite::default();
    let t0 = Instant::now();
    for i in 0..5u64 {
        assert_eq!(
            s.observer(Some(27.85), t0 + Duration::from_secs(30 * i)),
            LectureThermique::Lisible(27.85)
        );
    }
}

#[test]
fn une_zone_qui_bouge_reste_lisible() {
    let mut s = SuiviDeStabilite::default();
    let t0 = Instant::now();
    let mut derniere = LectureThermique::Lisible(0.0);
    for i in 0..30u64 {
        let t = 50.0 + (i % 7) as f64;
        derniere = s.observer(Some(t), t0 + Duration::from_secs(30 * i));
    }
    assert!(
        matches!(derniere, LectureThermique::Lisible(_)),
        "{derniere:?}"
    );
}

#[test]
fn les_compteurs_windows_se_convertissent_et_se_bornent() {
    // « High Precision Temperature » : dixièmes de kelvin.
    let c = celsius_depuis_dixiemes_de_kelvin(3_231.0).unwrap();
    assert!((c - 49.95).abs() < 1e-9, "{c}");
    // « Temperature » : kelvins entiers.
    let c = celsius_depuis_kelvins(323.0).unwrap();
    assert!((c - 49.85).abs() < 1e-9, "{c}");
    // Hors du domaine physique d'un capteur CPU : refusé, comme sous Linux.
    assert_eq!(celsius_depuis_kelvins(0.0), None);
    assert_eq!(
        celsius_depuis_kelvins(273.15),
        None,
        "0 °C n'est pas un processeur allumé"
    );
    assert_eq!(celsius_depuis_dixiemes_de_kelvin(50_000.0), None);
}
