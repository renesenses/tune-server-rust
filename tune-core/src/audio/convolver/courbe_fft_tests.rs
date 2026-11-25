//! La courbe par FFT contre des RI dont la réponse est connue.

use super::*;
use crate::audio::convolver::fir_frequency_response;

const FS: u32 = 48_000;

fn dirac(longueur: usize, position: usize) -> Vec<f32> {
    let mut h = vec![0.0f32; longueur];
    h[position] = 1.0;
    h
}

/// Passe-bas à phase linéaire : sinus cardinal fenêtré (Blackman), symétrique.
fn passe_bas(coupure_hz: f64, longueur: usize) -> Vec<f32> {
    let m = (longueur - 1) as f64;
    let fc = coupure_hz / f64::from(FS);
    (0..longueur)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
            (sinc * w) as f32
        })
        .collect()
}

fn au_plus_pres(c: &CourbeCanal, f: f64) -> usize {
    c.freqs_hz
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - f).abs().total_cmp(&(b.1 - f).abs()))
        .unwrap()
        .0
}

#[test]
fn la_grille_a_512_points_logarithmiques_de_20_hz_a_20_khz() {
    let c = courbe_du_canal(&dirac(16, 0), FS, POINTS_DE_LA_GRILLE).unwrap();
    assert_eq!(c.freqs_hz.len(), 512);
    assert!((c.freqs_hz[0] - 20.0).abs() < 1e-9);
    assert!((c.freqs_hz[511] - 20_000.0).abs() < 1e-6);
    let r = c.freqs_hz[1] / c.freqs_hz[0];
    assert!((c.freqs_hz[300] / c.freqs_hz[299] - r).abs() < 1e-9, "pas constant en log");
    // 44,1 kHz : 0,45·fs borne le haut ; 16 kHz de cadence aussi.
    let c = courbe_du_canal(&dirac(16, 0), 16_000, 512).unwrap();
    assert!((c.freqs_hz[511] - 7_200.0).abs() < 1e-6);
    assert!(courbe_du_canal(&[], FS, 512).is_none());
    assert!(courbe_du_canal(&[1.0], 0, 512).is_none());
}

#[test]
fn une_impulsion_retardee_a_une_phase_lineaire_et_une_amplitude_plate() {
    let d = 100;
    let c = courbe_du_canal(&dirac(4096, d), FS, 512).unwrap();
    assert_eq!(c.retard_echantillons, d);
    for (i, f) in c.freqs_hz.iter().enumerate() {
        assert!(c.magnitude_db[i].abs() < 1e-6, "{f} Hz : {}", c.magnitude_db[i]);
        // Retard retiré : phase nulle partout.
        let ex = c.phase_deroulee_deg[i].expect("phase publiée");
        assert!(ex.abs() < 1e-6, "{f} Hz : {ex}°");
    }
    // Avec le retard : une droite, −360·f·d/fs, déroulée sans saut.
    let brute = c.phase_deroulee_brute_deg(FS);
    for (i, f) in c.freqs_hz.iter().enumerate() {
        let attendu = -360.0 * f * d as f64 / f64::from(FS);
        assert!((brute[i].unwrap() - attendu).abs() < 1e-6, "{f} Hz");
        // La phase principale est la même, repliée.
        let p = c.phase_deg[i];
        let ecart = (p - attendu).rem_euclid(360.0);
        assert!(ecart < 1e-6 || ecart > 360.0 - 1e-6, "{f} Hz : {p} vs {attendu}");
    }
}

#[test]
fn un_passe_bas_a_phase_lineaire_coupe_au_bon_endroit_et_n_a_pas_de_phase_hors_retard() {
    let h = passe_bas(1_000.0, 1023);
    let c = courbe_du_canal(&h, FS, 512).unwrap();
    assert_eq!(c.retard_echantillons, 511, "le centre du FIR symétrique");
    let db = |f: f64| c.magnitude_db[au_plus_pres(&c, f)];
    assert!(db(100.0).abs() < 0.05, "bande passante : {}", db(100.0));
    assert!((db(1_000.0) + 6.0).abs() < 0.6, "−6 dB à la coupure : {}", db(1_000.0));
    assert!(db(5_000.0) < -70.0, "bande coupée : {}", db(5_000.0));
    // Bande passante : phase hors retard nulle (phase linéaire pure).
    for f in [50.0, 200.0, 500.0, 800.0] {
        let p = c.phase_deroulee_deg[au_plus_pres(&c, f)].unwrap();
        assert!(p.abs() < 0.5, "{f} Hz : {p}°");
    }
    // Bande coupée, sous le seuil : pas de phase publiée (bruit).
    assert!(c.phase_deroulee_deg[au_plus_pres(&c, 10_000.0)].is_none());
}

#[test]
fn la_fft_rend_la_meme_courbe_que_la_somme_directe() {
    // Un passe-bas d'ordre 1 tronqué (phase minimale, phase non nulle).
    let a = 0.95f32;
    let h: Vec<f32> = (0..2000).map(|n| a.powi(n)).collect();
    let c = courbe_du_canal(&h, FS, 512).unwrap();
    let directe = fir_frequency_response(&h, FS, &c.freqs_hz);
    for (i, p) in directe.iter().enumerate().step_by(7) {
        assert!(
            (c.magnitude_db[i] - p.magnitude_db).abs() < 0.05,
            "{} Hz : {} vs {}",
            p.freq_hz,
            c.magnitude_db[i],
            p.magnitude_db
        );
        let ecart = (c.phase_deg[i] - p.phase_deg).rem_euclid(360.0);
        assert!(ecart < 0.5 || ecart > 359.5, "{} Hz : phase {} vs {}", p.freq_hz, c.phase_deg[i], p.phase_deg);
    }
    // Retard 0 (pic au premier coefficient) : la phase déroulée est celle
    // du filtre analogique, −atan(a·sin ω / (1 − a·cos ω)), sans saut.
    for (i, f) in c.freqs_hz.iter().enumerate() {
        let w = 2.0 * std::f64::consts::PI * f / f64::from(FS);
        let a = f64::from(a);
        let attendu = -(a * w.sin()).atan2(1.0 - a * w.cos()).to_degrees();
        let p = c.phase_deroulee_deg[i].unwrap();
        assert!((p - attendu).abs() < 0.5, "{f} Hz : {p}° vs {attendu}°");
    }
}
