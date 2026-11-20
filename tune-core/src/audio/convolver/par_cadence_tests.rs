//! #8761 — choix du fichier par cadence, repli rééchantillonné, cache,
//! compatibilité d'un jeu à un seul fichier.

use super::*;
use crate::audio::convolver::{ConvolverConfig, fir_frequency_response};

fn ecrire(chemin: &std::path::Path, taps: &[f32], sample_rate: u32) {
    crate::audio::convolver::ecrire_wav_float32(chemin.to_str().unwrap(), taps, 1, sample_rate)
        .unwrap();
}

fn dirac(n: usize, position: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; n];
    v[position] = 1.0;
    v
}

/// Passe-bas à phase linéaire (sinc fenêtré de Hann), `n` impair.
fn passe_bas(n: usize, coupure_hz: f64, fs: u32) -> Vec<f32> {
    let fc = coupure_hz / fs as f64;
    let m = (n - 1) as f64 / 2.0;
    (0..n)
        .map(|i| {
            let x = i as f64 - m;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
            (sinc * hann) as f32
        })
        .collect()
}

fn ecart_de_phase(a: f64, b: f64) -> f64 {
    let mut d = (a - b) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d < -180.0 {
        d += 360.0;
    }
    d.abs()
}

/// Vérifie que `apres` (à `vers` Hz) reproduit `avant` (à `de` Hz) : même
/// module à ±0,1 dB et même phase à ±1° — sans retard : l'amorce est retirée
/// par `reechantillonner_ri` —, sur les fréquences où le filtre est
/// au-dessus de −40 dB, jusqu'à `f_max`.
fn meme_reponse(avant: &[f32], de: u32, apres: &[f32], vers: u32, f_max: f64) {
    let retard = 0usize;
    let freqs: Vec<f64> = (1..=200)
        .map(|k| 20.0 * (f_max / 20.0).powf(k as f64 / 200.0))
        .collect();
    let r1 = fir_frequency_response(avant, de, &freqs);
    let r2 = fir_frequency_response(apres, vers, &freqs);
    let mut points = 0;
    for (p1, p2) in r1.iter().zip(&r2) {
        if p1.magnitude_db < -40.0 {
            continue;
        }
        points += 1;
        let f = p1.freq_hz;
        assert!(
            (p1.magnitude_db - p2.magnitude_db).abs() < 0.1,
            "{de}→{vers} : module à {f:.0} Hz : {:.3} dB avant, {:.3} dB après",
            p1.magnitude_db,
            p2.magnitude_db
        );
        // Le retard d'amorce est un retard PUR : −360·f·retard/vers degrés.
        let attendu = p1.phase_deg - 360.0 * f * retard as f64 / vers as f64;
        let e = ecart_de_phase(p2.phase_deg, attendu);
        assert!(
            e < 1.0,
            "{de}→{vers} : phase à {f:.0} Hz décalée de {e:.3}°"
        );
    }
    assert!(points > 50, "trop peu de points mesurés ({points})");
}

#[test]
fn le_fichier_a_la_cadence_exacte_est_choisi_8761() {
    let d = crate::test_scratch::scratch_dir("ri-8761-jeu");
    let mut chemins = Vec::new();
    for sr in [48_000u32, 44_100, 96_000, 192_000] {
        let p = d.path().join(format!("f{sr}.wav"));
        ecrire(&p, &dirac(64, 3), sr);
        chemins.push(p.to_string_lossy().into_owned());
    }
    let jeu = ConvolverConfig::depuis_jeu(&chemins, true).unwrap();
    assert_eq!(jeu.cadences(), vec![48_000, 44_100, 96_000, 192_000]);
    assert_eq!(jeu.sample_rate(), 48_000, "la principale reste la première");
    for (i, sr) in [48_000u32, 44_100, 96_000, 192_000].into_iter().enumerate() {
        let choix = jeu.choix_pour(sr).unwrap();
        assert_eq!(choix.raison, "exact", "{sr}");
        assert!(!choix.reechantillonne, "{sr}");
        assert_eq!(choix.chemin.as_deref(), Some(chemins[i].as_str()), "{sr}");
        let (_, taps) = jeu.ri_pour(sr).unwrap();
        assert_eq!(taps[0], dirac(64, 3), "{sr} : fichier exact pris tel quel");
    }
}

#[test]
fn le_repli_prefere_la_meme_famille_puis_le_plus_proche_8761() {
    let jeu = |cadences: &[u32]| {
        let mut c = ConvolverConfig::new(vec![dirac(8, 0)], cadences[0]).unwrap();
        for &sr in &cadences[1..] {
            c = c.avec_variante(vec![dirac(8, 0)], sr).unwrap();
        }
        c
    };
    let cas: [(&[u32], u32, u32, &str); 7] = [
        (&[44_100, 96_000], 48_000, 96_000, "same_family"),
        (&[44_100, 96_000], 88_200, 44_100, "same_family"),
        (&[44_100, 96_000], 192_000, 96_000, "same_family"),
        (&[44_100, 96_000], 176_400, 44_100, "same_family"),
        (&[44_100], 48_000, 44_100, "nearest"),
        (&[44_100, 192_000], 96_000, 192_000, "same_family"),
        // Même famille et même écart (une octave) : on DESCEND.
        (&[48_000, 192_000], 96_000, 192_000, "same_family"),
    ];
    for (cadences, cible, attendu, raison) in cas {
        let choix = jeu(cadences).choix_pour(cible).unwrap();
        assert_eq!(
            (choix.cadence_fichier, choix.raison, choix.reechantillonne),
            (attendu, raison, true),
            "jeu {cadences:?}, flux {cible}"
        );
    }
}

#[test]
fn un_seul_fichier_marche_comme_avant_et_profite_du_repli_8761() {
    let f = crate::test_scratch::scratch_file("ri-8761-seul", ".wav");
    let ri = passe_bas(255, 8_000.0, 48_000);
    ecrire(f.path(), &ri, 48_000);
    let config = ConvolverConfig::from_wav(&f.as_str()).unwrap();
    // Même cadence : moteur bâti, fichier pris tel quel.
    let (choix, taps) = config.ri_pour(48_000).unwrap();
    assert!(!choix.reechantillonne);
    assert_eq!(&taps[0], &ri);
    assert_eq!(config.build_for(256, 48_000, 2).unwrap().channels(), 2);
    // Autre cadence : AVANT #8761, refus et piste jouée sans correction ;
    // maintenant, filtre rééchantillonné.
    let choix = config.choix_pour(96_000).unwrap();
    assert!(choix.reechantillonne);
    assert_eq!(choix.decrire(), "filtre rééchantillonné de 48 kHz à 96 kHz");
    assert_eq!(config.build_for(256, 96_000, 2).unwrap().channels(), 2);
    assert_eq!(dernier_choix(&f.as_str()), Some(choix));
    // Le refus reste possible par le réglage.
    let e = config
        .clone()
        .avec_reechantillonnage(false)
        .build_for(256, 96_000, 2)
        .err()
        .expect("refus attendu");
    assert!(e.contains("réexportez") && e.contains("96000"), "{e}");
}

#[test]
fn a_cadence_egale_le_fichier_par_cadence_remplace_le_principal_8761() {
    let d = crate::test_scratch::scratch_dir("ri-8761-remplace");
    let p = d.path().join("principal.wav");
    let s = d.path().join("supp.wav");
    let absent = d.path().join("absent.wav");
    ecrire(&p, &dirac(16, 0), 48_000);
    ecrire(&s, &dirac(16, 5), 48_000);
    let chemins = [p, absent, s].map(|c| c.to_string_lossy().into_owned());
    let jeu = ConvolverConfig::depuis_jeu(&chemins, true).unwrap();
    assert_eq!(
        jeu.cadences(),
        vec![48_000],
        "fichier illisible écarté, doublon remplacé"
    );
    let (choix, taps) = jeu.ri_pour(48_000).unwrap();
    assert_eq!(choix.chemin.as_deref(), Some(chemins[2].as_str()));
    assert_eq!(taps[0], dirac(16, 5));
}

#[test]
fn le_filtre_reechantillonne_est_mis_en_cache_8761() {
    let config = ConvolverConfig::new(vec![passe_bas(511, 5_000.0, 44_100)], 44_100).unwrap();
    let (_, a) = config.ri_pour(96_000).unwrap();
    let (_, b) = config.ri_pour(96_000).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "le second appel doit venir du cache");
    // Un clone de la configuration (ce que fait la sortie locale à chaque
    // piste) partage le même cache.
    let (_, c) = config.clone().ri_pour(96_000).unwrap();
    assert!(Arc::ptr_eq(&a, &c));
    // Une AUTRE RI à la même cadence n'y prend pas sa place.
    let autre = ConvolverConfig::new(vec![passe_bas(511, 5_000.0, 44_100)], 44_100).unwrap();
    let (_, d) = autre.ri_pour(96_000).unwrap();
    assert!(!Arc::ptr_eq(&a, &d));
}

#[test]
fn un_dirac_reechantillonne_garde_module_phase_et_longueur_8761() {
    for (de, vers) in [
        (48_000u32, 96_000u32),
        (96_000, 48_000),
        (44_100, 48_000),
        (44_100, 192_000),
    ] {
        let n = 4096;
        let ri = dirac(n, 1000);
        let apres = reechantillonner_ri(&[ri.clone()], de, vers);
        let (marge, retard) = marge_d_amorce(de, vers);
        let attendue =
            (((n + 2 * marge) as f64) * vers as f64 / de as f64).round() as usize - retard;
        assert_eq!(apres[0].len(), attendue, "{de}→{vers} : longueur");
        // Le pic tombe là où le temps le dit : 1000/de s, plus l'amorce.
        let pic = apres[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        // Même instant que dans le fichier d'origine : l'amorce est retirée.
        let attendu = 1000.0 * vers as f64 / de as f64;
        assert!(
            (pic as f64 - attendu).abs() <= 1.0,
            "{de}→{vers} : pic en {pic}, attendu {attendu}"
        );
        let f_max = 0.45 * de.min(vers) as f64;
        meme_reponse(&ri, de, &apres[0], vers, f_max.min(20_000.0));
    }
}

#[test]
fn un_passe_bas_reechantillonne_garde_sa_reponse_8761() {
    for (de, vers) in [
        (44_100u32, 96_000u32),
        (96_000, 44_100),
        (48_000, 44_100),
        (44_100, 88_200),
    ] {
        let ri = passe_bas(1023, 6_000.0, de);
        let apres = reechantillonner_ri(&[ri.clone()], de, vers);
        meme_reponse(&ri, de, &apres[0], vers, 19_000.0);
    }
}

/// Coût du rééchantillonnage d'une RI de 65 536 prises (stéréo). Mesure,
/// pas une garde : `cargo test --release -- --ignored --nocapture cout_`.
#[test]
#[ignore]
fn cout_du_reechantillonnage_de_65536_prises_8761() {
    let ri = passe_bas(65_535, 18_000.0, 44_100);
    let stereo = vec![ri.clone(), ri];
    for (de, vers) in [
        (44_100u32, 48_000u32),
        (44_100, 96_000),
        (48_000, 96_000),
        (44_100, 192_000),
        (96_000, 44_100),
    ] {
        let t = std::time::Instant::now();
        let r = reechantillonner_ri(&stereo, de, vers);
        eprintln!(
            "cout_8761 : 65 536 prises stéréo {de} → {vers} Hz : {:.1} ms ({} prises en sortie)",
            t.elapsed().as_secs_f64() * 1000.0,
            r[0].len()
        );
    }
}

/// Un WAV mono flottant écrit à la main, cadence quelconque (y compris des
/// valeurs qui déborderaient le calcul d'octets par seconde).
fn wav_brut(chemin: &std::path::Path, sample_rate: u32, prises: usize) {
    let donnees = (prises * 4) as u32;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + donnees).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&sample_rate.wrapping_mul(4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..prises {
        w.extend_from_slice(&(if i == 0 { 1.0f32 } else { 0.0 }).to_le_bytes());
    }
    std::fs::write(chemin, w).unwrap();
}

#[test]
fn une_frequence_hors_bornes_est_refusee_au_chargement_8761() {
    let d = crate::test_scratch::scratch_dir("ri-8761-bornes");
    for sr in [0u32, 1, 7_999, 768_001, 4_000_000_000] {
        let p = d.path().join(format!("f{sr}.wav"));
        wav_brut(&p, sr, 64);
        let e = ConvolverConfig::from_wav(p.to_str().unwrap())
            .err()
            .unwrap_or_else(|| panic!("{sr} Hz doit être refusé"));
        assert!(e.contains("hors des bornes") || e.contains("cadence"), "{sr} : {e}");
        assert!(ConvolverConfig::new(vec![vec![1.0]], sr).is_err(), "{sr} en mémoire");
    }
    for sr in [8_000u32, 768_000] {
        let p = d.path().join(format!("ok{sr}.wav"));
        wav_brut(&p, sr, 64);
        assert!(ConvolverConfig::from_wav(p.to_str().unwrap()).is_ok(), "{sr}");
    }
    // Dans un jeu, un fichier aberrant est écarté sans priver la zone des
    // autres ; en principal, il refuse le jeu (avertissement à l'appelant).
    let bon = d.path().join("ok8000.wav").to_string_lossy().into_owned();
    let mauvais = d.path().join("f1.wav").to_string_lossy().into_owned();
    let jeu = ConvolverConfig::depuis_jeu(&[bon.clone(), mauvais.clone()], true).unwrap();
    assert_eq!(jeu.cadences(), vec![8_000]);
    assert!(ConvolverConfig::depuis_jeu(&[mauvais, bon], true).is_err());
}

#[test]
fn une_ri_trop_longue_est_refusee_8761() {
    let d = crate::test_scratch::scratch_dir("ri-8761-longue");
    let p = d.path().join("longue.wav");
    wav_brut(&p, 48_000, PRISES_MAX + 1);
    let e = ConvolverConfig::from_wav(p.to_str().unwrap()).err().expect("refus");
    assert!(e.contains("prises"), "{e}");
    // Admise à sa cadence, mais trop longue une fois rééchantillonnée :
    // prises × rapport est plafonné, le moteur n'est pas bâti.
    let config = ConvolverConfig::new(vec![vec![0.0; PRISES_MAX]], 44_100).unwrap();
    assert!(config.build_for(1024, 44_100, 2).is_ok());
    let e = config.build_for(1024, 192_000, 2).err().expect("refus");
    assert!(e.contains("trop longue"), "{e}");
    assert!(octets_en_cache() <= 64 << 20);
}

#[test]
fn seuls_les_fichiers_de_la_zone_dans_ir_sont_effacables_8761() {
    let d = crate::test_scratch::scratch_dir("ri-8761-chemins");
    let ir = d.path().join("ir");
    let autre = d.path().join("autre");
    std::fs::create_dir_all(&ir).unwrap();
    std::fs::create_dir_all(&autre).unwrap();
    for f in ["zone_7.wav", "zone_7_sr96000.wav", "zone_8.wav", "notes.wav"] {
        std::fs::write(ir.join(f), b"x").unwrap();
    }
    std::fs::write(autre.join("zone_7.wav"), b"x").unwrap();
    let ok = |p: std::path::PathBuf| chemin_du_jeu_effacable(&p, &ir, 7);
    assert!(ok(ir.join("zone_7.wav")));
    assert!(ok(ir.join("zone_7_sr96000.wav")));
    assert!(ok(ir.join("..").join("ir").join("zone_7.wav")), "même dossier une fois résolu");
    assert!(!ok(ir.join("..").join("autre").join("zone_7.wav")), "« ../ » hors de ir/");
    assert!(!ok(autre.join("zone_7.wav")));
    assert!(!ok(ir.join("zone_8.wav")), "autre zone");
    assert!(!ok(ir.join("notes.wav")));
    assert!(!ok(std::path::PathBuf::from("/etc/passwd")));
    assert!(autre.join("zone_7.wav").exists(), "rien n'est supprimé par la garde");

    assert!(nom_du_jeu_valide("zone_7_sr44100.wav", Some(7)));
    assert!(!nom_du_jeu_valide("zone_7_sr.wav", Some(7)));
    assert!(!nom_du_jeu_valide("zone_7_srx.wav", None));
    assert!(!nom_du_jeu_valide("zone_7.wav.bak", None));

    let importee = liste_du_jeu_importable(
        r#"["/d/ir/zone_3_sr96000.wav", "/d/ir/../../etc/zone_3.wav", "/d/ir/passwd"]"#,
    );
    assert_eq!(importee.as_deref(), Some(r#"["/d/ir/zone_3_sr96000.wav"]"#));
    assert_eq!(liste_du_jeu_importable(r#"["../zone_1.wav"]"#), None);
    assert_eq!(liste_du_jeu_importable("pas du json"), None);
}
