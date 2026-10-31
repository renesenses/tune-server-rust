//! Les gabarits : ce que chaque conception PROMET, verrouillé. Un
//! changement de fenêtre, de portée ou de coupure qui casse une promesse
//! rougit ici, avec le chiffre mesuré dans le message.
//!
//! Contre-épreuve attendue (AGENTS.md) : abaisser `ATTENUATION_PRECISION_DB`
//! à 90 dans `conception.rs` doit faire rougir
//! `precision_tient_150_db_au_dela_de_nyquist` ; remplacer la fenêtre
//! gaussienne du « polyvalent » par Blackman-Harris doit faire rougir
//! `polyvalent_apodise_avant_nyquist`.

use tune_surechantillonnage::dither::{Quantificateur, formeur_par_id};
use tune_surechantillonnage::mesures::{
    attenuation_min_au_dela, bilan, bruit_de_quantification, gain_sinus_db, latence,
    rejet_des_images, reponse_frequentielle, thd_n_db,
};
use tune_surechantillonnage::{Surechantillonneur, par_id, plan, tester_la_machine_sur};

fn reponse(id: &str, de: u32, vers: u32) -> Vec<(f64, f64)> {
    let c = par_id(id).unwrap_or_else(|| panic!("conception {id}"));
    let p = plan(de, vers).unwrap();
    reponse_frequentielle(&c.noyau(p.l1), p.l1, de, 1 << 17)
}

#[test]
fn polyvalent_apodise_avant_nyquist() {
    let rep = reponse("polyvalent", 44_100, 352_800);
    let b = bilan(&rep, 44_100);
    assert!(b.f_moins_0_1_db >= 19_800.0, "−0,1 dB à {:.0} Hz", b.f_moins_0_1_db);
    assert!(b.ondulation_db <= 0.01, "ondulation {:.4} dB", b.ondulation_db);
    // Apodisant : déjà 100 dB AVANT Nyquist (modèle gaussien : −113 dB à
    // 21,9 kHz, −138 dB à 22,05 kHz).
    let a = attenuation_min_au_dela(&rep, 21_900.0);
    assert!(a >= 100.0, "atténuation au-delà de 21,9 kHz : {a:.1} dB");
    let a = attenuation_min_au_dela(&rep, 22_050.0);
    assert!(a >= 120.0, "atténuation au-delà de Nyquist : {a:.1} dB");
}

#[test]
fn polyvalent_reponse_hors_bande_monotone() {
    // Fenêtre gaussienne : pas de lobes qui remontent. La réponse au-delà
    // de Nyquist décroît, à 3 dB près, jusqu'à −140 dB (en dessous, la
    // troncature de la fenêtre laisse une ondulation résiduelle).
    let rep = reponse("polyvalent", 44_100, 352_800);
    let mut max_precedent = f64::INFINITY;
    for &(f, db) in rep.iter().filter(|p| p.0 >= 22_050.0 && p.1 > -140.0) {
        assert!(db <= max_precedent + 3.0, "remontée à {f:.0} Hz : {db:.1} dB");
        max_precedent = max_precedent.min(db);
    }
}

#[test]
fn precision_tient_150_db_au_dela_de_nyquist() {
    for (de, vers) in [(44_100, 352_800), (48_000, 384_000)] {
        let rep = reponse("precision", de, vers);
        let b = bilan(&rep, de);
        let nyq = de as f64 / 2.0;
        assert!(
            b.f_moins_0_1_db >= nyq - 500.0,
            "{de} : −0,1 dB à {:.0} Hz",
            b.f_moins_0_1_db
        );
        assert!(b.ondulation_db <= 1e-3, "{de} : ondulation {:.6} dB", b.ondulation_db);
        assert!(
            b.attenuation_au_dela_nyquist_db >= 145.0,
            "{de} : atténuation au-delà de Nyquist {:.1} dB",
            b.attenuation_au_dela_nyquist_db
        );
    }
}

#[test]
fn precision_longue_transition_plus_etroite() {
    let rep = reponse("precision-longue", 44_100, 352_800);
    let b = bilan(&rep, 44_100);
    assert!(b.f_moins_0_1_db >= 21_850.0, "−0,1 dB à {:.0} Hz", b.f_moins_0_1_db);
    assert!(b.attenuation_au_dela_nyquist_db >= 145.0, "{:.1} dB", b.attenuation_au_dela_nyquist_db);
}

#[test]
fn resolution_transition_etendue_et_courte() {
    let c = par_id("resolution").unwrap();
    assert!(c.portee <= 64, "portée {}", c.portee);
    let rep = reponse("resolution", 44_100, 352_800);
    let b = bilan(&rep, 44_100);
    assert!(b.f_moins_3_db >= 20_500.0, "−3 dB à {:.0} Hz", b.f_moins_3_db);
    assert!(
        (5.0..=7.0).contains(&b.attenuation_a_nyquist_db),
        "à Nyquist : {:.2} dB",
        b.attenuation_a_nyquist_db
    );
    assert!(
        b.attenuation_au_dela_1_5_nyquist_db >= 100.0,
        "au-delà de 33 kHz : {:.1} dB",
        b.attenuation_au_dela_1_5_nyquist_db
    );
}

#[test]
fn phase_minimale_meme_module_et_energie_devant() {
    for (lin, pm) in [
        ("polyvalent", "polyvalent-pm"),
        ("precision", "precision-pm"),
        ("resolution", "resolution-pm"),
    ] {
        let a = bilan(&reponse(lin, 44_100, 352_800), 44_100);
        let b = bilan(&reponse(pm, 44_100, 352_800), 44_100);
        assert!(
            (a.f_moins_3_db - b.f_moins_3_db).abs() < 50.0,
            "{pm} : −3 dB {:.0} vs {:.0}",
            b.f_moins_3_db,
            a.f_moins_3_db
        );
        assert!(
            b.attenuation_au_dela_1_5_nyquist_db >= a.attenuation_au_dela_1_5_nyquist_db.min(120.0) - 10.0,
            "{pm} : atténuation {:.1} vs {:.1}",
            b.attenuation_au_dela_1_5_nyquist_db,
            a.attenuation_au_dela_1_5_nyquist_db
        );
        let c = par_id(pm).unwrap();
        let mut sur = Surechantillonneur::nouveau(&c, 44_100, 352_800, 1).unwrap();
        let l = latence(&mut sur);
        assert!(
            l.moitie_energie_ms < 0.25 * l.theorique_ms,
            "{pm} : ½ énergie à {:.3} ms pour {:.3} ms en phase linéaire",
            l.moitie_energie_ms,
            l.theorique_ms
        );
    }
}

#[test]
fn latence_lineaire_mesuree_egale_la_theorie() {
    for (id, de, vers) in [("polyvalent", 44_100, 352_800), ("precision", 48_000, 384_000), ("polyvalent", 44_100, 384_000)] {
        let c = par_id(id).unwrap();
        let mut sur = Surechantillonneur::nouveau(&c, de, vers, 1).unwrap();
        let l = latence(&mut sur);
        assert!(
            (l.pic_ms - l.theorique_ms).abs() < 0.05,
            "{id} {de}→{vers} : pic {:.3} ms, théorie {:.3} ms",
            l.pic_ms,
            l.theorique_ms
        );
    }
}

#[test]
fn sinus_traverse_sans_perte_de_niveau() {
    for (de, vers) in [(48_000, 384_000), (44_100, 384_000), (48_000, 352_800), (44_100, 705_600)] {
        for id in ["polyvalent", "precision", "resolution", "precision-pm"] {
            let c = par_id(id).unwrap();
            let mut sur = Surechantillonneur::nouveau(&c, de, vers, 1).unwrap();
            let g = gain_sinus_db(&mut sur, 1000.0);
            assert!(g.abs() < 0.01, "{id} {de}→{vers} : {g:.4} dB à 1 kHz");
        }
    }
}

#[test]
fn rejet_des_images_par_balayage() {
    let attendu = [("polyvalent", 110.0), ("precision", 140.0), ("resolution", 25.0), ("polyvalent-pm", 105.0)];
    for (id, min_db) in attendu {
        for (de, vers) in [(44_100, 352_800), (44_100, 384_000)] {
            let c = par_id(id).unwrap();
            let mut sur = Surechantillonneur::nouveau(&c, de, vers, 1).unwrap();
            let r = rejet_des_images(&mut sur).rejet_db;
            assert!(r >= min_db, "{id} {de}→{vers} : rejet des images {r:.1} dB < {min_db}");
        }
    }
}

#[test]
fn thd_n_numerique_sous_le_plancher_24_bits() {
    let c = par_id("precision").unwrap();
    let mut sur = Surechantillonneur::nouveau(&c, 44_100, 352_800, 1).unwrap();
    let t = thd_n_db(&mut sur, 1000.0, 0.891, 20.0, 20_000.0, None);
    assert!(t < -160.0, "THD+N en f64 : {t:.1} dB");
    let mut sur = Surechantillonneur::nouveau(&c, 44_100, 352_800, 1).unwrap();
    let mut q = Quantificateur::nouveau(24, true, None, 1, 3);
    let t = thd_n_db(&mut sur, 1000.0, 0.891, 20.0, 20_000.0, Some(&mut q));
    // Théorie : pas²/4 × (19 980 / 176 400) rapporté à (0,891²/2) = −149,9 dB.
    assert!(t < -144.0 && t > -156.0, "THD+N 24 bits TPDF : {t:.1} dB");
}

#[test]
fn tpdf_16_bits_au_niveau_theorique() {
    // Variance de l'erreur = pas²/12 (arrondi) + pas²/6 (TPDF) = pas²/4.
    let mut q = Quantificateur::nouveau(16, true, None, 1, 11);
    let b = bruit_de_quantification(44_100, &mut q, 4.0);
    let attendu = 20.0 * (q.pas() / 2.0).log10();
    assert!((b.total_dbfs - attendu).abs() < 0.3, "{:.2} dBFS vs {attendu:.2}", b.total_dbfs);
}

#[test]
fn formeurs_abaissent_le_bruit_audible() {
    let reference = |cadence: u32| {
        let mut q = Quantificateur::nouveau(16, true, None, 1, 5);
        bruit_de_quantification(cadence, &mut q, 4.0)
    };
    let r44 = reference(44_100);
    for (id, gain_min) in [("wannamaker9f", 10.0), ("wannamaker9e", 10.0), ("lipshitz5e", 6.0)] {
        let f = formeur_par_id(id).unwrap();
        let mut q = Quantificateur::nouveau(16, true, Some(&f), 1, 5);
        let b = bruit_de_quantification(44_100, &mut q, 4.0);
        let gain = r44.pondere_ath_db - b.pondere_ath_db;
        assert!(gain >= gain_min, "{id} à 44,1 kHz : gain pondéré {gain:.1} dB < {gain_min}");
        assert!(b.total_dbfs > r44.total_dbfs, "{id} : le bruit total doit monter (Gerzon-Craven)");
    }
    let r352 = reference(352_800);
    for (id, gain_min) in [("ordre1", 8.0), ("ordre2", 20.0), ("ordre3", 28.0), ("ordre4", 36.0)] {
        let f = formeur_par_id(id).unwrap();
        let mut q = Quantificateur::nouveau(16, true, Some(&f), 1, 5);
        let b = bruit_de_quantification(352_800, &mut q, 4.0);
        let gain = r352.bande_audible_dbfs - b.bande_audible_dbfs;
        assert!(gain >= gain_min, "{id} à 352,8 kHz : gain 20 Hz–20 kHz {gain:.1} dB < {gain_min}");
    }
}

#[test]
fn test_de_la_machine_rend_un_resultat_par_couple() {
    let c = par_id("resolution").unwrap();
    let r = tester_la_machine_sur(&[c], &[(44_100, 352_800), (48_000, 352_800)], 2, 0.05, 3.0);
    assert_eq!(r.resultats.len(), 2);
    for x in &r.resultats {
        assert!(x.facteur_temps_reel.is_finite() && x.facteur_temps_reel > 0.0);
        assert!(x.gmac_s > 0.0);
    }
    assert_eq!(r.admis_pour(44_100, 352_800).len(), usize::from(r.resultats[0].admis));
}
