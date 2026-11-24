//! #9472 (fils du forum 3041 et 3032) — une musique PLEINE BANDE mais calme,
//! dont l'énergie est surtout dans le grave, était déclarée « spectre coupé
//! net vers 1 kHz » : la falaise se cherchait dès 1 kHz, par un écart RELATIF
//! raie par raie, et un aigu large bande 45 dB sous des notes graves y passait
//! pour une coupure. Signaux synthétiques produits par le test ; aucun
//! fichier, aucun son joué.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;
use tune_core::audio::faux_hires::{COUPURE_FALAISE_MIN_HZ, Raison, Verdict, juger, mesurer};

/// Générateur pseudo-aléatoire déterministe (LCG), dans [-1, 1].
struct Lcg(u64);
impl Lcg {
    fn suivant(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// Quantifie sur `bits` bits (arrondi), en entiers comme les rend le décodeur.
fn quantifier(signal: &[f32], bits: u32) -> Vec<i32> {
    let pleine = (1i64 << (bits - 1)) as f32;
    signal.iter().map(|s| (s * pleine).round() as i32).collect()
}

/// Des notes graves (fondamentales de 110 à 440 Hz et leur 2e harmonique,
/// toutes sous 900 Hz) vers -12 dBFS de crête, plus un tapis LARGE BANDE (bruit
/// blanc dérivé : plein de 1 kHz à Nyquist) `tapis_db` plus bas. Tout est
/// audible : il n'y a aucune coupure.
fn grave_et_calme(sample_rate: u32, secondes: f64, tapis_db: f64, graine: u64) -> Vec<f32> {
    let n = (sample_rate as f64 * secondes) as usize;
    let notes = [110.0, 164.8, 220.0, 329.6, 440.0];
    let mut v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            let x: f64 = notes
                .iter()
                .enumerate()
                .map(|(k, f0)| {
                    (std::f64::consts::TAU * f0 * t + k as f64).sin()
                        + 0.3 * (std::f64::consts::TAU * 2.0 * f0 * t).sin()
                })
                .sum();
            x / 6.5 * 0.25
        })
        .collect();
    let gain = 10f64.powf(tapis_db / 20.0) * 0.25;
    let mut alea = Lcg(graine);
    let mut precedent = 0.0;
    for s in v.iter_mut() {
        let b = alea.suivant();
        *s += gain * (b - precedent) * 0.5;
        precedent = b;
    }
    v.iter().map(|&x| x as f32).collect()
}

/// Un bruit ROSE de 20 Hz à `bande_hz`, rien au-dessus (une vraie coupure),
/// normalisé à -6 dBFS de crête.
fn rose_coupe(sample_rate: u32, bande_hz: f64, graine: u64) -> Vec<f32> {
    par_spectre(sample_rate, graine, |f| {
        if (20.0..bande_hz).contains(&f) {
            1.0 / f.sqrt()
        } else {
            0.0
        }
    })
}

/// Un bruit d'amplitude `amplitude(f)`, phases aléatoires, normalisé à
/// -6 dBFS de crête.
fn par_spectre(sample_rate: u32, graine: u64, amplitude: impl Fn(f64) -> f64) -> Vec<f32> {
    let n = 1usize << 17;
    let inverse = RealFftPlanner::<f32>::new().plan_fft_inverse(n);
    let mut spectre = inverse.make_input_vec();
    let mut alea = Lcg(graine);
    let hz_par_raie = sample_rate as f64 / n as f64;
    for (k, c) in spectre.iter_mut().enumerate() {
        let f = k as f64 * hz_par_raie;
        let phase = (alea.suivant() * std::f64::consts::PI) as f32;
        let a = amplitude(f) as f32;
        *c = Complex::new(a * phase.cos(), a * phase.sin());
    }
    let dernier = spectre.len() - 1;
    spectre[dernier].im = 0.0;
    let mut sortie = inverse.make_output_vec();
    inverse.process(&mut spectre, &mut sortie).unwrap();
    let crete = sortie.iter().fold(0f32, |m, s| m.max(s.abs()));
    sortie.iter().map(|s| s / crete * 0.5).collect()
}

/// 🔴 TÉMOIN #9472 — le cas des fils 3041 et 3032 : avant le correctif, ce
/// 24/96 était rendu `Douteux(CoupureSpectrale)` avec une coupure vers
/// 1 043 Hz, toutes ses fenêtres en cause. Il est vérifié, sans coupure
/// suspecte, et aussi en 24/44,1 et pour un tapis de -25 à -70 dB.
#[test]
fn une_musique_grave_et_calme_n_est_pas_coupee_3041() {
    for (sample_rate, attendu) in [(96_000u32, Verdict::Verifie), (44_100, Verdict::Verifie)] {
        for tapis in [-25.0, -45.0, -70.0] {
            let pcm = quantifier(&grave_et_calme(sample_rate, 12.0, tapis, 7), 24);
            let m = mesurer(&pcm, 1, sample_rate, 24);
            assert!(m.fenetres_jugees > 0, "{m:?}");
            assert!(
                m.fenetres_coupees.is_empty(),
                "{sample_rate} Hz, tapis {tapis} dB : aucune fenêtre ne doit être en cause — {m:?}"
            );
            assert!(
                m.coupure_hz.is_none_or(|f| f >= COUPURE_FALAISE_MIN_HZ),
                "{sample_rate} Hz, tapis {tapis} dB : coupure {:?}",
                m.coupure_hz
            );
            assert_eq!(
                juger(&m),
                attendu,
                "{sample_rate} Hz, tapis {tapis} dB : {m:?}"
            );
        }
    }
}

/// 🟢 CONTRE-ÉPREUVE #9472 — une VRAIE coupure juste au-dessus de la borne
/// (11 kHz, un MP3 à 64 kbit/s) reste vue dans un 24/96.
#[test]
fn une_vraie_coupure_a_11_khz_reste_vue_9472() {
    let pcm = quantifier(&rose_coupe(96_000, 11_000.0, 9472), 24);
    let m = mesurer(&pcm, 1, 96_000, 24);
    let f = m
        .coupure_hz
        .unwrap_or_else(|| panic!("coupure attendue : {m:?}"));
    assert!((f - 11_000.0).abs() < 400.0, "coupure {f:.0} Hz : {m:?}");
    assert!(
        matches!(
            juger(&m),
            Verdict::Douteux(Raison::CoupureSpectrale { .. } | Raison::SignatureDeCodec { .. })
        ),
        "{m:?}"
    );
}

/// 🟢 #9472 — sous la borne, une coupure n'est pas jugée : une falaise à
/// 5 kHz (bien plus bas que tout passe-bas d'encodeur courant) ne rend pas le
/// fichier douteux.
#[test]
fn une_falaise_sous_la_borne_n_est_pas_une_coupure_9472() {
    assert_eq!(COUPURE_FALAISE_MIN_HZ, 10_000.0);
    let pcm = quantifier(&rose_coupe(96_000, 5_000.0, 9473), 24);
    let m = mesurer(&pcm, 1, 96_000, 24);
    assert!(m.fenetres_coupees.is_empty(), "{m:?}");
    assert_eq!(juger(&m), Verdict::Verifie, "{m:?}");
}

/// 🔴 TÉMOIN #9472 (relecture de #9474) — un MP3 à bas débit d'un master
/// SOMBRE, en 24/96 : bande passante forte jusqu'à 8,5 kHz, aigu 40 dB plus
/// bas au-dessus, la VRAIE coupure de l'encodeur à 12 kHz, puis le bruit de
/// fond de la chaîne (50 dB sous l'aigu) jusqu'à Nyquist. La bande forte,
/// majoritaire sous la falaise, tire la référence du bord : la remontée du
/// bord s'arrête vers 8,5 kHz, sous la borne. La falaise à 12 kHz, elle, est
/// réelle et doit rester vue : rendre `None` à cause du bord (e0f7e1999)
/// faisait passer ce fichier « vérifié ». Paramètres trouvés par une sonde :
/// avec une coupure à 14 kHz ou plus haut, la bande forte n'est plus
/// majoritaire et le bord reste au-dessus de la borne.
#[test]
fn un_master_sombre_coupe_haut_reste_douteux_9472() {
    let aigu = 10f64.powf(-40.0 / 20.0);
    let fond = aigu / 12_000f64.sqrt() * 10f64.powf(-50.0 / 20.0);
    let pcm = quantifier(
        &par_spectre(96_000, 9474, |f| {
            if f < 20.0 {
                0.0
            } else if f < 8_500.0 {
                1.0 / f.sqrt()
            } else if f < 12_000.0 {
                aigu / f.sqrt()
            } else {
                fond
            }
        }),
        24,
    );
    let m = mesurer(&pcm, 1, 96_000, 24);
    assert!(
        !m.fenetres_coupees.is_empty(),
        "la coupure à 12 kHz doit être vue — {m:?}"
    );
    let f = m
        .coupure_hz
        .unwrap_or_else(|| panic!("coupure attendue : {m:?}"));
    assert!(
        (COUPURE_FALAISE_MIN_HZ..20_000.0).contains(&f),
        "coupure {f:.0} Hz : {m:?}"
    );
    assert!(
        matches!(
            juger(&m),
            Verdict::Douteux(Raison::CoupureSpectrale { .. } | Raison::SignatureDeCodec { .. })
        ),
        "{m:?}"
    );
}
