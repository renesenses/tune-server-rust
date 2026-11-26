//! rc14 (Didier) — la marge d'un filtre de convolution qui amplifie : des
//! MESURES sur la sortie du moteur, pas des lectures de réglage.

use super::*;
use crate::tess::chaine::{HEADROOM_AUTO_DB, HEADROOM_MANUEL_MIN_DB, Headroom};
use crate::tess::etages_fixes::{MARGE_DE_SECURITE_CONVOLUTION_DB, marge_de_convolution};

const FS: u32 = 48_000;
const BLOC: usize = 1024;

/// Réponse impulsionnelle d'un égaliseur en cloche (RBJ), `prises` prises.
fn cloche(fs: u32, f0: f64, gain_db: f64, q: f64, prises: usize) -> Vec<f32> {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / f64::from(fs);
    let alpha = w0.sin() / (2.0 * q);
    let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a);
    let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a);
    let (b0, b1, b2, a1, a2) = (b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0);
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    (0..prises)
        .map(|i| {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            y as f32
        })
        .collect()
}

/// Un sinus stéréo entrelacé (`f64`), amplitude `a` (1 = 0 dBFS).
fn sinus(fs: u32, f: f64, secondes: f64, a: f64) -> Vec<f64> {
    let n = (f64::from(fs) * secondes) as usize;
    let mut v = Vec::with_capacity(2 * n);
    for i in 0..n {
        let x = a * (2.0 * std::f64::consts::PI * f * i as f64 / f64::from(fs)).sin();
        v.push(x);
        v.push(x);
    }
    v
}

fn crete(v: &[f64]) -> f64 {
    v.iter().fold(0.0f64, |m, x| m.max(x.abs()))
}

fn stereo(h: Vec<f32>) -> Vec<Vec<f32>> {
    vec![h.clone(), h]
}

/// Le filtre plat (Dirac, 0 dB) : la marge reste celle de l'Auto (0 dB
/// depuis #9520), les partitions sont intactes et la sortie est celle du
/// moteur d'avant, AU BIT PRÈS.
#[test]
fn filtre_plat_marge_inchangee_et_sortie_au_bit_pres() {
    let mut h = vec![0.0f32; 4096];
    h[0] = 1.0;
    let config = ConvolverConfig::new(stereo(h.clone()), FS).unwrap();
    let marge = config.marge_pour(FS).unwrap();
    assert!(marge.gain_max_db.abs() < 1e-9, "{marge:?}");
    assert_eq!(marge.appliquee_db, HEADROOM_AUTO_DB);
    assert_eq!(marge.requise_db, 0.0);
    let mut protege = config.build_for(BLOC, FS, 2).unwrap();
    assert_eq!(protege.reserve_db(), 0.0);
    let mut brut = Convolver::new(&stereo(h), BLOC);
    let mut a = sinus(FS, 997.0, 0.5, 1.0);
    let mut b = a.clone();
    protege.process_interleaved_f64(&mut a);
    brut.process_interleaved_f64(&mut b);
    assert!(
        a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()),
        "le filtre plat ne doit rien changer"
    );
    // Le bilan de niveau : rien de plus hors convolution (bit-perfect).
    let bilan = crate::tess::etages_fixes::BilanDeNiveau::calculer(
        &crate::tess::etages_fixes::EntreesNiveau {
            headroom: Headroom::Auto {},
            headroom_resolu_db: 0.0,
            replaygain_db: 0.0,
            gain_de_zone_db: 0.0,
            gain_moyen_greffons_db: 0.0,
            compensation_active: true,
            gain_max_convolution_db: marge.gain_max_db,
        },
    );
    assert_eq!(bilan.headroom_db, 0.0);
    assert_eq!(bilan.gain_d_entree_db, 0.0);
}

/// Le cas de Didier : +6 dB à 50 Hz, sinus de 50 Hz à 0 dBFS. Avec la marge,
/// aucune crête au-dessus de 0 dBFS ; le même filtre SANS marge dépasse
/// d'environ +6 dB (contre-épreuve dans le même test).
#[test]
fn filtre_plus_6_db_a_50_hz_aucune_crete_au_dessus_de_0_dbfs() {
    let h = cloche(FS, 50.0, 6.0, 1.0, 1 << 15);
    let config = ConvolverConfig::new(stereo(h.clone()), FS).unwrap();
    let marge = config.marge_pour(FS).unwrap();
    assert!(
        (marge.gain_max_db - 6.0).abs() < 0.05,
        "gain maximal mesuré : {marge:?}"
    );
    assert!(
        (marge.appliquee_db - -(marge.gain_max_db + MARGE_DE_SECURITE_CONVOLUTION_DB)).abs()
            < 1e-12
    );
    let mut protege = config.build_for(BLOC, FS, 2).unwrap();
    assert_eq!(protege.reserve_db(), marge.appliquee_db);
    let mut y = sinus(FS, 50.0, 3.0, 1.0);
    protege.process_interleaved_f64(&mut y);
    let c = crete(&y);
    assert!(c <= 1.0, "crête {:.3} dBFS", 20.0 * c.log10());
    assert!(c > 0.85, "la marge ne doit pas écraser le signal : {c}");

    // Contre-épreuve : le moteur sans marge écrête de +6 dB.
    let mut brut = Convolver::new(&stereo(h), BLOC);
    let mut z = sinus(FS, 50.0, 3.0, 1.0);
    brut.process_interleaved_f64(&mut z);
    let cz = crete(&z);
    assert!(
        20.0 * cz.log10() > 5.5,
        "sans marge, la crête doit dépasser 0 dBFS : {cz}"
    );
}

/// Un filtre qui atténue partout ne reçoit aucune marge de plus que l'Auto.
#[test]
fn filtre_qui_attenue_partout_pas_plus_que_l_auto() {
    let h = cloche(FS, 1_000.0, -9.0, 0.7, 8192)
        .into_iter()
        .map(|x| x * 0.5)
        .collect::<Vec<_>>();
    let config = ConvolverConfig::new(stereo(h), FS).unwrap();
    let marge = config.marge_pour(FS).unwrap();
    assert!(marge.gain_max_db < -5.0, "{marge:?}");
    assert_eq!(marge.requise_db, 0.0);
    assert!(marge.appliquee_db >= HEADROOM_AUTO_DB.min(-3.0));
    assert_eq!(marge.appliquee_db, HEADROOM_AUTO_DB);
    assert_eq!(config.build_for(BLOC, FS, 2).unwrap().reserve_db(), 0.0);
}

/// Après rééchantillonnage vers une autre cadence, le gain est RECALCULÉ sur
/// le filtre rééchantillonné : une bosse de +6 dB à 30 kHz dans un filtre à
/// 96 kHz disparaît à 44,1 kHz (au-dessus de Nyquist).
#[test]
fn gain_recalcule_apres_reechantillonnage() {
    let h = cloche(96_000, 30_000.0, 6.0, 2.0, 8192);
    let config = ConvolverConfig::new(stereo(h), 96_000).unwrap();
    let a_96 = config.marge_pour(96_000).unwrap();
    let a_44 = config.marge_pour(44_100).unwrap();
    assert!(a_96.gain_max_db > 5.5, "{a_96:?}");
    assert!(a_44.gain_max_db < a_96.gain_max_db - 2.0, "{a_44:?}");
    let (_, taps) = config.ri_pour(44_100).unwrap();
    let attendu = courbe_fft::gain_max_db(&taps, 44_100).unwrap();
    assert_eq!(
        a_44.gain_max_db, attendu,
        "le gain de la RI rééchantillonnée"
    );
    assert_eq!(
        config.build_for(BLOC, 44_100, 2).unwrap().reserve_db(),
        a_44.appliquee_db
    );

    // Et la protection tient à la cadence d'arrivée : +6 dB à 50 Hz à
    // 48 kHz, joué à 96 kHz.
    let h = cloche(FS, 50.0, 6.0, 1.0, 1 << 15);
    let config = ConvolverConfig::new(stereo(h), FS).unwrap();
    // Le gain est celui de la RI RÉÉCHANTILLONNÉE (mesurée, pas supposée :
    // un filtre dont le pic est en tête ne garde pas exactement son gain au
    // passage 48 → 96 kHz).
    let m = config.marge_pour(96_000).unwrap();
    let (_, t96) = config.ri_pour(96_000).unwrap();
    assert_eq!(
        m.gain_max_db,
        courbe_fft::gain_max_db(&t96, 96_000).unwrap(),
        "{m:?}"
    );
    assert!(m.gain_max_db > 0.0 && m.appliquee_db < 0.0, "{m:?}");
    let mut moteur = config.build_for(BLOC, 96_000, 2).unwrap();
    let mut y = sinus(96_000, 50.0, 3.0, 1.0);
    moteur.process_interleaved_f64(&mut y);
    assert!(crete(&y) <= 1.0, "crête {}", crete(&y));
}

/// Retirer un filtre à marge : le fondu de 20 ms garde la marge sur le sec
/// (aucune hausse en 20 ms), puis le sec remonte en environ 0,5 s vers le
/// niveau d'avant le filtre, jamais au-dessus ; une borne de volume l'arrête
/// plus bas.
#[test]
fn retrait_du_filtre_remonte_en_un_demi_seconde_sans_depasser() {
    let h = vec![2.0f32]; // +6,02 dB partout
    let config = ConvolverConfig::new(stereo(h), FS).unwrap();
    for (borne, cible) in [(1.0, 1.0), (0.7, 0.7)] {
        let mut m = config.build_for(BLOC, FS, 2).unwrap();
        let r = 10f64.powf(m.reserve_db() / 20.0);
        assert!(r < 0.5, "marge {} dB", m.reserve_db());
        let a = 0.9;
        let mut amorce = sinus(FS, 1_000.0, 0.2, a);
        m.process_interleaved_f64(&mut amorce);
        let avant = crete(&amorce[amorce.len() / 2..]);
        assert!(avant <= a, "le filtre protégé reste sous le sec : {avant}");
        m.borner_la_remontee(borne);
        m.eteindre_en_fondu((FS / 50) as usize);
        let mut y = sinus(FS, 1_000.0, 1.0, a);
        m.process_interleaved_f64(&mut y);
        // Crête par fenêtre de 10 ms.
        let fenetre = (FS / 100) as usize * 2;
        let cretes: Vec<f64> = y.chunks(fenetre).map(crete).collect();
        assert!(
            cretes.iter().all(|c| *c <= avant.max(a * cible) + 1e-9),
            "jamais au-dessus du niveau d'avant : {cretes:?}"
        );
        assert!(
            cretes[3..].iter().all(|c| *c <= a * cible + 1e-9),
            "jamais au-dessus de la borne : {cretes:?}"
        );
        // Fondu fini (30 ms) : le sec porte encore la marge, il n'a pas
        // sauté au niveau d'avant.
        assert!(cretes[3] < a * (r + 0.05), "{cretes:?}");
        // À 100 ms, encore loin de la cible ; à 600 ms, arrivé.
        assert!(cretes[10] < a * cible * 0.9, "{cretes:?}");
        assert!((cretes[60] - a * cible).abs() < 1e-3, "{cretes:?}");
        // Montée lente : aucune fenêtre ne gagne plus de 1,5 dB sur la précédente.
        for w in cretes[3..60].windows(2) {
            assert!(20.0 * (w[1] / w[0]).log10() < 1.5, "{cretes:?}");
        }
        assert!((m.gain_sec() - cible).abs() < 1e-12);
    }
}

/// La règle elle-même : Auto, borne à −24 dB ; en manuel, M une seule fois
/// (revue de #9561 : par le convolueur quand aucun égaliseur ne la porte).
#[test]
fn regle_de_la_marge_auto_et_manuelle() {
    let auto = marge_de_convolution(&Headroom::Auto {}, 6.0, None);
    assert_eq!(auto.appliquee_db, -6.5);
    assert!(!auto.insuffisante);
    assert_eq!(
        marge_de_convolution(&Headroom::Auto {}, 40.0, None).appliquee_db,
        HEADROOM_MANUEL_MIN_DB
    );
    assert_eq!(
        marge_de_convolution(&Headroom::Auto {}, f64::NAN, None).appliquee_db,
        0.0
    );
    // L'étage des égaliseurs porte M : le convolueur ne la repose pas.
    let manuel = marge_de_convolution(&Headroom::Manual { db: -3.0 }, 6.0, Some(-3.0));
    assert_eq!(manuel.appliquee_db, 0.0, "jamais deux fois");
    assert!(manuel.insuffisante);
    assert_eq!(manuel.requise_db, -6.5);
    let assez = marge_de_convolution(&Headroom::Manual { db: -8.0 }, 6.0, Some(-8.0));
    assert!(!assez.insuffisante);
    assert!(!marge_de_convolution(&Headroom::Manual { db: 0.0 }, -2.0, None).insuffisante);
    // Le moteur suit le mode de la chaîne : sans étage d'égaliseur connu,
    // c'est lui qui porte M.
    let h = cloche(FS, 50.0, 6.0, 1.0, 1 << 14);
    let config = ConvolverConfig::new(stereo(h), FS)
        .unwrap()
        .avec_headroom(Headroom::Manual { db: -3.0 });
    let m = config.marge_pour(FS).unwrap();
    assert!(m.insuffisante && m.appliquee_db == -3.0, "{m:?}");
    assert_eq!(config.build_for(BLOC, FS, 2).unwrap().reserve_db(), -3.0);
}
