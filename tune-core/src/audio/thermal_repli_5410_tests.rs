//! #5410 / #6176 — la garde thermique des analyses hors Linux.
//!
//! Didier (fil 2239, Windows 11, Ryzen 7 3800X) : un PC arrêté en surchauffe
//! pendant le calcul de la plage dynamique. Sous Windows et macOS la
//! température n'était jamais lue : la garde « arrêt à 80 °C, reprise à
//! 70 °C » ne protégeait rien, et les analyses tournaient à pleine largeur.
//!
//! Ces témoins portent sur la DÉCISION, en logique pure, testable partout :
//! * sans température lisible, les analyses de fond passent à UN fichier à la
//!   fois, quelle que soit la vitesse réglée ;
//! * une température lisible laisse la vitesse réglée intacte (Linux, ou
//!   Windows avec une zone thermique) ;
//! * une zone thermique FIGÉE (même valeur pendant longtemps : la zone ACPI de
//!   beaucoup de cartes de bureau) ne vaut pas une mesure — repli aussi ;
//! * la conversion des compteurs Windows (dixièmes de kelvin, kelvins).
//!
//! La lecture Windows elle-même (PDH) ne se teste pas ici : elle se compile
//! pour `x86_64-pc-windows-msvc`, son comportement reste à mesurer sur une
//! machine Windows.

use std::time::{Duration, Instant};

use super::{
    LectureThermique, MotifIllisible, SuiviDeStabilite, brider_sans_thermometre,
    celsius_depuis_dixiemes_de_kelvin, celsius_depuis_kelvins, largeur_selon_la_chaleur,
};

#[test]
fn sans_temperature_lisible_les_analyses_passent_a_un_fichier() {
    let illisible = LectureThermique::Illisible(MotifIllisible::AucunCapteur);
    assert_eq!(brider_sans_thermometre(2, &illisible), 1, "normal (2) → 1");
    assert_eq!(brider_sans_thermometre(4, &illisible), 1, "rapide (4) → 1");
    assert_eq!(
        brider_sans_thermometre(1, &illisible),
        1,
        "discret reste à 1"
    );
    let figee = LectureThermique::Illisible(MotifIllisible::Figee);
    assert_eq!(
        brider_sans_thermometre(4, &figee),
        1,
        "une zone figée bride aussi"
    );
}

/// #6713 — le bridage levé par l'utilisateur rend la vitesse réglée quand la
/// température est ILLISIBLE (aucun capteur ou zone figée), et seulement
/// alors. Sans le réglage, c'est le bridage de #5410 (#6450).
#[test]
fn le_bridage_leve_rend_la_vitesse_reglee_sans_temperature_6713() {
    for motif in [MotifIllisible::AucunCapteur, MotifIllisible::Figee] {
        let illisible = LectureThermique::Illisible(motif);
        assert_eq!(
            largeur_selon_la_chaleur(4, &illisible, true),
            4,
            "{motif:?}"
        );
        assert_eq!(
            largeur_selon_la_chaleur(2, &illisible, true),
            2,
            "{motif:?}"
        );
        assert_eq!(
            largeur_selon_la_chaleur(0, &illisible, true),
            1,
            "jamais zéro"
        );
        assert_eq!(
            largeur_selon_la_chaleur(4, &illisible, false),
            1,
            "{motif:?} : sans le réglage, le bridage par défaut reste"
        );
    }
    // Une température LUE ne dépend pas du réglage : la largeur réglée, et
    // c'est la garde chaude (`ThermalGate`) qui arrête au-delà de 80 °C.
    let chaude = LectureThermique::Lisible(85.0);
    assert_eq!(largeur_selon_la_chaleur(4, &chaude, true), 4);
    assert_eq!(largeur_selon_la_chaleur(4, &chaude, false), 4);
}

#[test]
fn une_temperature_lisible_laisse_la_vitesse_reglee() {
    let lisible = LectureThermique::Lisible(55.0);
    assert_eq!(brider_sans_thermometre(2, &lisible), 2);
    assert_eq!(brider_sans_thermometre(4, &lisible), 4);
    assert_eq!(brider_sans_thermometre(1, &lisible), 1);
    // Jamais zéro, même sur une entrée dégénérée.
    assert_eq!(brider_sans_thermometre(0, &lisible), 1);
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
