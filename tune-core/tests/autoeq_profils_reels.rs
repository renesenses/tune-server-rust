//! L'import AutoEq, éprouvé sur de VRAIS fichiers publiés — #1405.
//!
//! Les trois fixtures de `tests/fixtures/autoeq/` sont les fichiers
//! `… ParametricEQ.txt` du dépôt <https://github.com/jaakkopasanen/AutoEq>,
//! repris tels quels :
//!
//! - `sennheiser_hd_650.txt` — `results/oratory1990/over-ear/Sennheiser HD 650/`
//! - `akg_k701.txt` — `results/oratory1990/over-ear/AKG K701/`
//! - `etymotic_er4sr.txt` — `results/crinacle/711 in-ear/Etymotic ER4SR/`
//!
//! Un test sur du texte écrit à la main prouverait seulement que l'analyseur
//! lit ce que l'analyseur écrit. Ces trois-là prouvent qu'il lit AutoEq.

use tune_core::audio::autoeq::{ErreurAutoEq, analyser};
use tune_core::audio::eq::{EqProcessor, EqProfile};

const HD_650: &str = include_str!("fixtures/autoeq/sennheiser_hd_650.txt");
const K701: &str = include_str!("fixtures/autoeq/akg_k701.txt");
const ER4SR: &str = include_str!("fixtures/autoeq/etymotic_er4sr.txt");

fn tous() -> [(&'static str, &'static str); 3] {
    [
        ("Sennheiser HD 650", HD_650),
        ("AKG K701", K701),
        ("Etymotic ER4SR", ER4SR),
    ]
}

#[test]
fn les_trois_profils_publies_sont_lus_en_dix_bandes() {
    for (nom, texte) in tous() {
        let profil = analyser(texte).unwrap_or_else(|e| panic!("{nom} : {e}"));
        assert_eq!(
            profil.bandes.len(),
            10,
            "{nom} : AutoEq exporte dix filtres"
        );
        assert!(profil.preamp_db < 0.0, "{nom} : le Preamp est negatif");
        // Les exports d'AutoEq n'ont aucun filtre desactive : le compte rendu
        // le dit, et l'utilisateur ne cherche pas de bande manquante.
        assert_eq!(
            profil.filtres_ignores, 0,
            "{nom} : aucun filtre OFF dans un export AutoEq"
        );
    }
}

/// Les filtres `OFF` sont ecartes du son, mais COMPTES dans le compte rendu.
///
/// Equalizer APO exporte volontiers des lignes desactivees. Les taire ferait
/// croire a une troncature : dix lignes dans le fichier, sept bandes dans le
/// prereglage, et rien pour expliquer l'ecart.
#[test]
fn les_filtres_desactives_dun_profil_reel_sont_comptes() {
    let avec_off = HD_650
        .replace("Filter 8: ON", "Filter 8: OFF")
        .replace("Filter 9: ON", "Filter 9: OFF")
        .replace("Filter 10: ON", "Filter 10: OFF");
    let profil = analyser(&avec_off).unwrap();
    assert_eq!(profil.bandes.len(), 7);
    assert_eq!(profil.filtres_ignores, 3);
    // Et les sept bandes restantes sont bien les sept premieres du fichier.
    assert_eq!(profil.bandes[6].freq, 1227.0);
}

/// La traduction, valeur par valeur, sur un fichier entier.
///
/// Le HD 650 est repris ici en entier plutot qu'echantillonne : c'est le seul
/// moyen de voir qu'aucune ligne n'est sautee, decalee ou arrondie.
#[test]
fn le_hd_650_est_traduit_ligne_a_ligne() {
    let profil = analyser(HD_650).unwrap();
    assert_eq!(profil.preamp_db, -6.1);

    let attendu: [(&str, f64, f64, f64); 10] = [
        ("low_shelf", 105.0, 6.4, 0.70),
        ("peak", 8800.0, 5.1, 1.42),
        ("peak", 118.0, -3.1, 0.50),
        ("peak", 37.0, 0.7, 3.96),
        ("peak", 3169.0, -1.7, 3.89),
        ("high_shelf", 10000.0, -2.1, 0.70),
        ("peak", 1227.0, -1.2, 2.53),
        ("peak", 2055.0, 1.2, 3.23),
        ("peak", 587.0, 0.4, 1.19),
        ("peak", 5332.0, -1.1, 5.75),
    ];

    for (i, (band_type, freq, gain, q)) in attendu.iter().enumerate() {
        let bande = &profil.bandes[i];
        assert_eq!(&bande.band_type, band_type, "bande {}", i + 1);
        assert_eq!(bande.freq, *freq, "bande {} : Fc", i + 1);
        assert_eq!(bande.gain, *gain, "bande {} : Gain", i + 1);
        assert_eq!(bande.q, *q, "bande {} : Q", i + 1);
        assert_eq!(bande.channel, None, "bande {} : les deux oreilles", i + 1);
    }
}

/// LE point de vigilance : un profil AutoEq pousse, et sans marge il ecrete.
///
/// Tune reserve la somme des gains positifs
/// (`EqProfile::automatic_headroom_db`, d423c16b). Cette somme majore toujours
/// le maximum de la reponse combinee, que le `Preamp` d'AutoEq vient
/// compenser : la marge reservee est donc au moins aussi protectrice que celle
/// que le fichier demande. Ce test le VERIFIE sur les trois profils plutot que
/// de le supposer.
#[test]
fn la_marge_reservee_par_tune_couvre_le_preamp_de_chaque_profil() {
    for (nom, texte) in tous() {
        let profil = analyser(texte).unwrap();
        let reservee = profil.marge_reservee_db();
        assert!(
            reservee <= profil.preamp_db,
            "{nom} : Tune reserve {reservee:.1} dB, AutoEq demande {:.1} dB",
            profil.preamp_db
        );
        assert!(profil.marge_de_tune_couvre_le_preamp(), "{nom}");
    }
}

/// La consequence, chiffree, de cette marge plus severe.
///
/// Elle n'est pas un defaut — rien n'ecrete — mais elle s'entend : un profil
/// AutoEq joue plus bas dans Tune que dans un lecteur qui applique le `Preamp`
/// du fichier. L'ecart va de 5 a 16 dB sur ces trois casques. Ce test le fige
/// pour que personne ne decouvre le chiffre a l'oreille.
#[test]
fn l_ecart_entre_la_marge_de_tune_et_le_preamp_est_connu_et_chiffre() {
    let mesures: [(&str, &str, f64, f64); 3] = [
        ("Sennheiser HD 650", HD_650, -6.1, -13.8),
        ("AKG K701", K701, -6.1, -16.7),
        ("Etymotic ER4SR", ER4SR, -6.4, -22.2),
    ];
    for (nom, texte, preamp, marge) in mesures {
        let profil = analyser(texte).unwrap();
        assert!(
            (profil.preamp_db - preamp).abs() < 1e-9,
            "{nom} : Preamp lu {}",
            profil.preamp_db
        );
        assert!(
            (profil.marge_reservee_db() - marge).abs() < 1e-9,
            "{nom} : marge reservee {}",
            profil.marge_reservee_db()
        );
    }
}

/// Les bandes importees atteignent bien un egaliseur ACTIF.
///
/// Un import qui produirait des bandes que `EqProcessor` juge neutres serait
/// un import sans effet — le defaut exact de #1718, dans une autre variante.
#[test]
fn un_profil_importe_produit_un_egaliseur_actif_qui_modifie_le_signal() {
    for (nom, texte) in tous() {
        let importe = analyser(texte).unwrap();
        let profil = EqProfile {
            enabled: true,
            bands: importe.bandes,
            ..Default::default()
        };
        let mut processeur = EqProcessor::new(&profil, 44100, 2);
        assert!(
            processeur.is_enabled(),
            "{nom} : l'egaliseur doit etre actif"
        );

        // Un sinus a 1 kHz, 24 bits stereo, doit ressortir different.
        let mut pcm = sinus_24_bits_stereo(1000.0, 4096, 44100);
        let avant = pcm.clone();
        processeur.process_pcm(&mut pcm, 24);
        assert_ne!(pcm, avant, "{nom} : le signal doit etre modifie");

        // Et sans deborder : c'est ce que la marge reservee garantit.
        assert_eq!(
            processeur.process_stats().overs,
            0,
            "{nom} : aucun echantillon hors plage"
        );
    }
}

/// Un fichier malforme est refuse, pas transforme en bandes absurdes.
///
/// La fixture reelle est mutilee de trois manieres differentes ; chacune doit
/// produire une erreur nommee, pas un profil approximatif.
#[test]
fn une_fixture_reelle_mutilee_est_refusee_proprement() {
    // 1. Un type de filtre que l'egaliseur ne sait pas construire.
    let mutile = HD_650.replace("ON LSC", "ON BP");
    assert!(matches!(
        analyser(&mutile),
        Err(ErreurAutoEq::TypeInconnu { ligne: 2, .. })
    ));

    // 2. Une frequence a zero — sans refus, `coeffs` la remonterait a 10 Hz.
    let mutile = HD_650.replace("Fc 8800 Hz", "Fc 0 Hz");
    assert!(matches!(
        analyser(&mutile),
        Err(ErreurAutoEq::ValeurHorsDomaine {
            ligne: 3,
            champ: "Fc",
            ..
        })
    ));

    // 3. Un gain illisible.
    let mutile = HD_650.replace("Gain 6.4 dB", "Gain ?? dB");
    assert!(matches!(
        analyser(&mutile),
        Err(ErreurAutoEq::NombreInvalide {
            ligne: 2,
            champ: "Gain",
            ..
        })
    ));
}

fn sinus_24_bits_stereo(freq: f64, trames: usize, taux: u32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(trames * 6);
    for n in 0..trames {
        let t = n as f64 / f64::from(taux);
        // -6 dBFS : de la marge dans la source, comme un vrai enregistrement.
        let valeur =
            (0.5 * (2.0 * std::f64::consts::PI * freq * t).sin() * f64::from(0x7F_FFFF)) as i32;
        for _ in 0..2 {
            pcm.extend_from_slice(&valeur.to_le_bytes()[..3]);
        }
    }
    pcm
}
