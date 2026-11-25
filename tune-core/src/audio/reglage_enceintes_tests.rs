//! Témoins du réglage des enceintes (`speaker-setup`, rc14).

use super::*;
use serde_json::json;

fn voie(distance_cm: f64, gain_db: f64, invert: bool) -> VoieEnceinte {
    VoieEnceinte {
        distance_cm,
        gain_db,
        invert,
    }
}

fn reglages(voies: Vec<VoieEnceinte>, invert_all: bool) -> ReglagesEnceintes {
    ReglagesEnceintes {
        channels: voies,
        invert_all,
    }
}

/// Un bruit déterministe, pleine échelle 24 bits exacte en `f32`.
fn bruit(n: usize, graine: u32) -> Vec<f32> {
    let mut x = graine.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            ((x >> 8) as i32 - (1 << 23)) as f32 / 8_388_608.0
        })
        .collect()
}

#[test]
fn le_delai_est_exact_en_echantillons_a_44_1_96_et_192_khz() {
    // 343 cm d'écart = 10 ms tout rond : 441, 960 et 1 920 échantillons.
    let r = reglages(vec![voie(343.0, 0.0, false), voie(0.0, 0.0, false)], false);
    for (cadence, attendu) in [(44_100, 441), (96_000, 960), (192_000, 1_920)] {
        assert_eq!(r.retards_en_echantillons(2, cadence), vec![0, attendu]);
        assert_eq!(r.latence_trames(2, cadence), attendu);
        // Et le processeur le fait : une impulsion sur la voie proche sort
        // exactement `attendu` trames plus tard, intacte.
        let mut p = ProcesseurEnceintes::new(r.clone());
        let trames = attendu + 10;
        let mut t = vec![0.0_f64; trames * 2];
        t[0] = 0.75; // voie lointaine, trame 0
        t[1] = -0.5; // voie proche, trame 0
        assert!(p.traiter(&mut t, 2, cadence));
        assert_eq!(t[0], 0.75, "la voie la plus lointaine n'a aucun délai");
        for n in 0..trames {
            let attendu_proche = if n == attendu { -0.5 } else { 0.0 };
            assert_eq!(t[n * 2 + 1], attendu_proche, "{cadence} Hz, trame {n}");
        }
    }
    // Un écart qui ne tombe pas juste est ARRONDI au plus proche, jamais
    // interpolé : 100 cm à 44,1 kHz = 128,57 → 129.
    let r = reglages(vec![voie(400.0, 0.0, false), voie(300.0, 0.0, false)], false);
    assert_eq!(r.retards_en_echantillons(2, 44_100), vec![0, 129]);
    let ms = r.retards_ms(2);
    assert_eq!(ms[0], 0.0);
    assert!((ms[1] - 2.915_451_9).abs() < 1e-6, "{}", ms[1]);
}

#[test]
fn reglages_neutres_le_flux_reste_bit_perfect() {
    let original = bruit(4_096 * 2, 7);
    for r in [
        ReglagesEnceintes::default(),
        reglages(vec![voie(250.0, 0.0, false), voie(250.0, 0.0, false)], false),
        // Deux inversions s'annulent : aucune inversion nette.
        reglages(vec![voie(300.0, 0.0, true), voie(300.0, 0.0, true)], true),
        // Le même gain partout est repris par la réserve : facteur 1.
        reglages(vec![voie(300.0, 4.0, false), voie(300.0, 4.0, false)], false),
    ] {
        assert!(r.est_neutre(2), "{r:?}");
        let mut p = ProcesseurEnceintes::new(r.clone());
        let mut t = original.clone();
        assert!(!p.traiter(&mut t, 2, 44_100), "rien n'est touché : {r:?}");
        // Neutre pour ces deux voies (l'inversion globale toucherait une
        // troisième voie non réglée : c'est le format vu qui compte).
        assert!(p.neutre_au_repos());
        assert_eq!(
            t.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            original.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn un_delai_seul_deplace_les_echantillons_sans_les_alterer() {
    let r = reglages(vec![voie(0.0, 0.0, false), voie(343.0, 0.0, false)], false);
    let d = r.retards_en_echantillons(2, 96_000)[0];
    assert_eq!(d, 960);
    let original = bruit(8_192 * 2, 11);
    let mut p = ProcesseurEnceintes::new(r);
    // En plusieurs blocs : la ligne à retard passe d'un bloc à l'autre.
    let mut sortie = Vec::new();
    for bloc in original.chunks(1_000 * 2) {
        let mut b = bloc.to_vec();
        p.traiter(&mut b, 2, 96_000);
        sortie.extend(b);
    }
    for n in 0..8_192 {
        // Voie 1 (la plus lointaine) : intacte, au bit près.
        assert_eq!(sortie[n * 2 + 1].to_bits(), original[n * 2 + 1].to_bits());
        // Voie 0 : la même suite, décalée de `d` trames, au bit près.
        let attendu = if n < d { 0.0 } else { original[(n - d) * 2] };
        assert_eq!(sortie[n * 2].to_bits(), attendu.to_bits(), "trame {n}");
    }
}

#[test]
fn gain_par_voie_avec_reserve_pour_le_positif() {
    // −6 dB sur la voie 1 : la voie 0 reste EXACTE, la voie 1 descend.
    let r = reglages(vec![voie(300.0, 0.0, false), voie(300.0, -6.0, false)], false);
    assert_eq!(r.reserve_db(2), 0.0);
    let f = r.facteurs(2);
    assert_eq!(f[0], 1.0);
    assert!((f[1] - 10f64.powf(-6.0 / 20.0)).abs() < 1e-15);
    let mut p = ProcesseurEnceintes::new(r);
    let mut t = vec![0.5_f64, 0.5, -0.25, -0.25];
    assert!(p.traiter(&mut t, 2, 48_000));
    assert_eq!(t[0], 0.5);
    assert_eq!(t[2], -0.25);
    assert!((t[1] - 0.5 * 10f64.powf(-0.3)).abs() < 1e-12);

    // +6 dB sur la voie 0 : la réserve (−6 dB) la ramène à 0 dB ; la voie 1,
    // à 0 dB demandé, sort à −6 dB. L'écart demandé est tenu, et aucune voie
    // ne dépasse l'entrée : pas d'écrêtage possible.
    let r = reglages(vec![voie(300.0, 6.0, false), voie(300.0, 0.0, false)], false);
    assert_eq!(r.reserve_db(2), -6.0);
    let f = r.facteurs(2);
    assert_eq!(f[0], 1.0);
    assert!((20.0 * (f[0] / f[1]).log10() - 6.0).abs() < 1e-12);
    let mut p = ProcesseurEnceintes::new(r);
    let mut t = vec![1.0_f64, 1.0];
    p.traiter(&mut t, 2, 48_000);
    assert!(t.iter().all(|x| x.abs() <= 1.0), "{t:?}");
}

#[test]
fn inversion_par_voie_et_inversion_globale() {
    let original = bruit(512 * 2, 3);
    let cas = [
        // (inversion voie 0, voie 1, globale) → signe de chaque voie
        ((false, true, false), [1.0, -1.0]),
        ((false, false, true), [-1.0, -1.0]),
        ((true, false, true), [1.0, -1.0]),
        ((true, true, false), [-1.0, -1.0]),
    ];
    for ((i0, i1, tout), signes) in cas {
        let r = reglages(vec![voie(300.0, 0.0, i0), voie(300.0, 0.0, i1)], tout);
        let mut p = ProcesseurEnceintes::new(r);
        let mut t = original.clone();
        assert!(p.traiter(&mut t, 2, 44_100));
        for (i, (y, x)) in t.iter().zip(&original).enumerate() {
            // Une inversion est exacte : −x, au bit près.
            assert_eq!(y.to_bits(), (x * signes[i % 2] as f32).to_bits());
        }
    }
}

#[test]
fn le_nombre_de_voies_qui_arrivent_est_libre() {
    // Six voies réglées (5.1) : FL, FR, C, LFE, SL, SR.
    let six = reglages(
        vec![
            voie(300.0, 0.0, false),
            voie(300.0, 0.0, false),
            voie(265.7, -2.0, false),
            voie(231.4, 3.0, true),
            voie(197.1, 0.0, false),
            voie(197.1, 0.0, false),
        ],
        false,
    );
    // 6 voies : délais 0, 0, 1, 2, 3, 3 ms à 48 kHz (34,3 cm par ms).
    assert_eq!(
        six.retards_en_echantillons(6, 48_000),
        vec![0, 0, 48, 96, 144, 144]
    );
    // 2 voies seulement (repli en amont) : seules les deux premières entrées
    // comptent, et elles sont à la même distance.
    assert_eq!(six.retards_en_echantillons(2, 48_000), vec![0, 0]);
    assert!(six.est_neutre(2));
    // 1 voie : rien à aligner.
    assert!(six.est_neutre(1));
    // Plus de voies que de réglages : les voies en trop sont neutres.
    let deux = reglages(vec![voie(100.0, 0.0, false), voie(443.0, 0.0, false)], false);
    assert_eq!(
        deux.retards_en_echantillons(6, 44_100),
        vec![441, 0, 0, 0, 0, 0]
    );

    for canaux in [1u16, 2, 6] {
        let mut p = ProcesseurEnceintes::new(six.clone());
        let n = usize::from(canaux);
        let mut t = vec![0.25_f32; 256 * n];
        let touche = p.traiter(&mut t, canaux, 48_000);
        assert_eq!(touche, canaux == 6, "{canaux} voies");
    }
    // Le même processeur suit un changement de nombre de voies.
    let mut p = ProcesseurEnceintes::new(six);
    let mut t6 = vec![0.25_f32; 256 * 6];
    assert!(p.traiter(&mut t6, 6, 48_000));
    let mut t2 = vec![0.25_f32; 256 * 2];
    assert!(!p.traiter(&mut t2, 2, 48_000));
    assert!(t2.iter().all(|x| *x == 0.25));
}

/// Le plus grand saut entre deux échantillons successifs d'une voie.
fn plus_grand_saut(t: &[f64], canaux: usize, voie: usize) -> f64 {
    t.iter()
        .skip(voie)
        .step_by(canaux)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max)
}

#[test]
fn un_changement_a_chaud_passe_par_un_fondu_sans_clic() {
    let cadence = 48_000;
    let rampe = (RAMPE_MS / 1000.0 * f64::from(cadence)) as usize;
    // Un continu de 0,5 : sans fondu, passer de 0 à −20 dB ferait une marche
    // de 0,45 d'un échantillon à l'autre.
    let mut p = ProcesseurEnceintes::new(reglages(
        vec![voie(300.0, 0.0, false), voie(300.0, -0.5, false)],
        false,
    ));
    let mut avant = vec![0.5_f64; 1_000 * 2];
    p.traiter(&mut avant, 2, cadence);
    p.definir(reglages(
        vec![voie(300.0, -20.0, false), voie(300.0, -0.5, false)],
        false,
    ));
    let mut apres = vec![0.5_f64; (rampe + 1_000) * 2];
    p.traiter(&mut apres, 2, cadence);
    let mut tout = avant.clone();
    tout.extend(&apres);
    let saut = plus_grand_saut(&tout, 2, 0);
    assert!(saut < 0.45 / rampe as f64 * 1.01, "saut {saut}");
    // Au bout du fondu : le nouveau réglage, exactement.
    let fin = apres[apres.len() - 2];
    assert!((fin - 0.05).abs() < 1e-12, "{fin}");

    // Un changement de DISTANCE : le délai saute de 0 à 480 trames ; le
    // fondu enchaîne l'ancienne et la nouvelle sortie sur un sinus lent.
    let sinus = |n: usize| (n as f64 * 2.0 * std::f64::consts::PI * 50.0 / 48_000.0).sin() * 0.8;
    let mut p = ProcesseurEnceintes::new(reglages(
        vec![voie(300.0, 0.0, false), voie(300.0, 0.0, true)],
        false,
    ));
    let mut sortie = Vec::new();
    let mut n = 0;
    for bloc in 0..20 {
        if bloc == 10 {
            p.definir(reglages(
                vec![voie(300.0, 0.0, false), voie(643.0, 0.0, true)],
                false,
            ));
        }
        let mut t: Vec<f64> = (0..512).flat_map(|i| [sinus(n + i), sinus(n + i)]).collect();
        p.traiter(&mut t, 2, cadence);
        n += 512;
        sortie.extend(t);
    }
    // Le pas naturel d'un sinus de 50 Hz à 0,8 est ~0,0052 par échantillon ;
    // une coupure franche ferait un saut jusqu'à 1,6.
    let saut = plus_grand_saut(&sortie, 2, 0);
    assert!(saut < 0.01, "saut {saut}");
}

#[test]
fn un_reglage_recu_pendant_un_fondu_attend_sa_fin() {
    let cadence = 1_000; // 20 trames de fondu
    let mut p = ProcesseurEnceintes::new(reglages(vec![voie(300.0, 0.0, false)], false));
    let mut t = vec![1.0_f64; 4];
    p.traiter(&mut t, 1, cadence);
    p.definir(reglages(vec![voie(300.0, 0.0, false)], true));
    let mut t = vec![1.0_f64; 5];
    p.traiter(&mut t, 1, cadence);
    // En plein fondu : le troisième réglage est mis en attente.
    p.definir(ReglagesEnceintes::default());
    assert!(!p.neutre_au_repos());
    let mut t = vec![1.0_f64; 200];
    p.traiter(&mut t, 1, cadence);
    // Les deux fondus se sont enchaînés, et l'on revient à l'identité.
    assert_eq!(*t.last().unwrap(), 1.0);
    assert!(p.neutre_au_repos());
    assert!(plus_grand_saut(&t, 1, 0) <= 2.0 / 20.0 + 1e-12);
}

#[test]
fn les_reglages_sont_lus_et_bornes() {
    let ok = json!({
        "enabled": true,
        "channels": [
            { "distance_cm": 312.5, "gain_db": -1.5, "invert": false },
            { "distance_cm": 280, "gain_db": 2, "invert": true }
        ],
        "invert_all": false
    });
    let r = ReglagesEnceintes::lire(&ok).unwrap();
    assert_eq!(r.channels.len(), 2);
    assert!(ReglagesEnceintes::lire(&Value::Null).unwrap().est_neutre_partout());
    // Valeurs par défaut d'une voie incomplète.
    let r = ReglagesEnceintes::lire(&json!({ "channels": [{}] })).unwrap();
    assert_eq!(r.channels[0], VoieEnceinte::default());
    for mauvais in [
        json!({ "channels": [{ "distance_cm": -1 }] }),
        json!({ "channels": [{ "distance_cm": 2000.5 }] }),
        json!({ "channels": [{ "gain_db": 10.5 }] }),
        json!({ "channels": [{ "gain_db": -20.5 }] }),
        json!({ "channels": [{ "delay_ms": 3 }] }),
        json!({ "invert": true }),
        json!([1, 2]),
        json!({ "channels": vec![json!({}); VOIES_MAX + 1] }),
    ] {
        assert!(ReglagesEnceintes::lire(&mauvais).is_err(), "{mauvais}");
    }
    // Le schéma publie les mêmes bornes.
    let s = schema_reglages();
    let p = &s["properties"]["channels"]["items"]["properties"];
    assert_eq!(p["distance_cm"]["minimum"], json!(DISTANCE_MIN_CM));
    assert_eq!(p["distance_cm"]["maximum"], json!(DISTANCE_MAX_CM));
    assert_eq!(p["gain_db"]["minimum"], json!(GAIN_MIN_DB));
    assert_eq!(p["gain_db"]["maximum"], json!(GAIN_MAX_DB));
    assert_eq!(s["properties"]["channels"]["maxItems"], json!(VOIES_MAX));
    assert_eq!(s["additionalProperties"], json!(false));
}

#[test]
fn coupe_sous_pure_et_sous_bypass_all() {
    use crate::db::settings_repo::SettingsRepo;
    let base = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    crate::db::migrations::run_migrations(&base).unwrap();
    let db: std::sync::Arc<dyn crate::db::backend::DbBackend> = std::sync::Arc::new(base);
    let s = SettingsRepo::with_backend(db.clone());
    let z = 7;
    let reglage = json!({
        "enabled": true,
        "channels": [{ "distance_cm": 300 }, { "distance_cm": 200, "gain_db": -3 }]
    });
    s.set(&cle_de_zone(z), &reglage.to_string()).unwrap();
    assert!(reglages_appliques(&db, z).is_some(), "armé, appliqué");

    // PURE : coupé.
    s.set(&format!("zone_{z}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert!(reglages_configures(&s, z).is_some());
    assert!(reglages_appliques(&db, z).is_none(), "PURE le coupe");
    s.delete(&format!("zone_{z}_audiophile")).unwrap();
    assert!(reglages_appliques(&db, z).is_some());

    // « Bypass all » de la chaîne TESS : coupé.
    s.set(
        &crate::tess::cle_chaine_zone(z),
        r#"{"version":1,"bypass_all":true,"plugins":[]}"#,
    )
    .unwrap();
    assert!(reglages_appliques(&db, z).is_none(), "Bypass all le coupe");
    s.set(
        &crate::tess::cle_chaine_zone(z),
        r#"{"version":1,"bypass_all":false,"plugins":[]}"#,
    )
    .unwrap();
    assert!(reglages_appliques(&db, z).is_some());

    // Décoché : rien.
    s.set(&cle_de_zone(z), r#"{"enabled":false}"#).unwrap();
    assert!(reglages_appliques(&db, z).is_none());
}
