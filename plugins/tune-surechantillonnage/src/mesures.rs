//! Le banc de mesure : réponses impulsionnelle et fréquentielle, atténuation
//! hors bande, repliement/images par balayage, THD+N, bruit après
//! quantification, latence.
//!
//! Tout est en f64 ; les générateurs sont déterministes. Les fonctions
//! prennent un suréchantillonneur NEUF : son état n'est pas remis à zéro.

use std::f64::consts::PI;

use crate::dither::{Quantificateur, ath_terhardt_db};
use crate::fft::{dsp_welch, module_spectre, puissance_de_deux};
use crate::polyphase::Surechantillonneur;

/// Réponse en fréquence d'un noyau pris à `l × de` hertz : `(Hz, dB)` de 0 à
/// `l × de / 2`, module normalisé au gain continu `l`.
pub fn reponse_frequentielle(noyau: &[f64], l: usize, de: u32, nfft: usize) -> Vec<(f64, f64)> {
    let nfft = puissance_de_deux(nfft.max(noyau.len() * 4));
    let m = module_spectre(noyau, nfft);
    let cadence = l as f64 * de as f64;
    m.iter()
        .enumerate()
        .map(|(k, &v)| {
            let db = 20.0 * (v / l as f64).max(1e-20).log10();
            (k as f64 * cadence / nfft as f64, db)
        })
        .collect()
}

/// Les chiffres qui résument une réponse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bilan {
    pub f_moins_0_1_db: f64,
    pub f_moins_3_db: f64,
    pub f_moins_6_db: f64,
    /// Plus grand écart au 0 dB entre 0 et 20 kHz (ou `f_moins_0_1_db` si
    /// elle est plus basse).
    pub ondulation_db: f64,
    /// Atténuation minimale (dB, positive) pour `f ≥ de/2`.
    pub attenuation_au_dela_nyquist_db: f64,
    /// Atténuation minimale pour `f ≥ 1,5 × de/2`.
    pub attenuation_au_dela_1_5_nyquist_db: f64,
    /// Atténuation à `de/2` exactement.
    pub attenuation_a_nyquist_db: f64,
}

/// Première fréquence où la réponse passe sous `seuil_db`.
pub fn frequence_a(rep: &[(f64, f64)], seuil_db: f64) -> f64 {
    for w in rep.windows(2) {
        if w[1].1 < seuil_db {
            // Interpolation linéaire entre les deux points.
            let (f0, d0) = w[0];
            let (f1, d1) = w[1];
            if d0 <= seuil_db {
                return f0;
            }
            return f0 + (seuil_db - d0) / (d1 - d0) * (f1 - f0);
        }
    }
    rep.last().map_or(0.0, |p| p.0)
}

/// Atténuation minimale (dB, positive) sur `f ≥ f_hz`.
pub fn attenuation_min_au_dela(rep: &[(f64, f64)], f_hz: f64) -> f64 {
    rep.iter()
        .filter(|p| p.0 >= f_hz)
        .map(|p| -p.1)
        .fold(f64::INFINITY, f64::min)
}

/// Niveau (dB) à la fréquence la plus proche de `f_hz`.
pub fn niveau_a(rep: &[(f64, f64)], f_hz: f64) -> f64 {
    rep.iter()
        .min_by(|a, b| (a.0 - f_hz).abs().partial_cmp(&(b.0 - f_hz).abs()).unwrap())
        .map_or(f64::NAN, |p| p.1)
}

pub fn bilan(rep: &[(f64, f64)], de: u32) -> Bilan {
    let nyq = de as f64 / 2.0;
    let f01 = frequence_a(rep, -0.1);
    let borne = f01.min(20_000.0);
    let ondulation = rep
        .iter()
        .filter(|p| p.0 <= borne)
        .map(|p| p.1.abs())
        .fold(0.0, f64::max);
    Bilan {
        f_moins_0_1_db: f01,
        f_moins_3_db: frequence_a(rep, -3.0),
        f_moins_6_db: frequence_a(rep, -6.0),
        ondulation_db: ondulation,
        attenuation_au_dela_nyquist_db: attenuation_min_au_dela(rep, nyq),
        attenuation_au_dela_1_5_nyquist_db: attenuation_min_au_dela(rep, 1.5 * nyq),
        attenuation_a_nyquist_db: -niveau_a(rep, nyq),
    }
}

/// Réponse impulsionnelle `(ms, amplitude)` du noyau à `l × de`.
pub fn impulsion(noyau: &[f64], l: usize, de: u32) -> Vec<(f64, f64)> {
    let cadence = l as f64 * de as f64;
    noyau
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64 * 1000.0 / cadence, v))
        .collect()
}

/// Sinusoïde d'amplitude crête `a`.
pub fn sinus(cadence: u32, f: f64, a: f64, n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| a * (2.0 * PI * f * i as f64 / cadence as f64).sin())
        .collect()
}

/// Balayage exponentiel de `f0` à `f1`, amplitude crête `a`, avec un fondu
/// de 10 ms aux deux bouts.
pub fn balayage(cadence: u32, f0: f64, f1: f64, a: f64, duree_s: f64) -> Vec<f64> {
    let n = (duree_s * cadence as f64) as usize;
    let k = (f1 / f0).ln();
    let fondu = (0.01 * cadence as f64) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / cadence as f64;
            let phase = 2.0 * PI * f0 * duree_s / k * ((k * t / duree_s).exp() - 1.0);
            let g = if i < fondu {
                i as f64 / fondu as f64
            } else if i + fondu > n {
                (n - i) as f64 / fondu as f64
            } else {
                1.0
            };
            a * g * phase.sin()
        })
        .collect()
}

/// Résultat du balayage : rapport (dB) entre l'énergie de sortie au-dessus de
/// `de/2` (images) et celle en dessous, et le spectre de sortie en dB
/// relatif au maximum.
pub struct Images {
    pub rejet_db: f64,
    pub spectre: Vec<(f64, f64)>,
}

/// Balayage de 20 Hz à 20 kHz (mono) à travers `sur`, 3 s, Welch 65 536.
pub fn rejet_des_images(sur: &mut Surechantillonneur) -> Images {
    assert_eq!(sur.canaux(), 1, "mesure mono");
    let plan = sur.plan();
    let entree = balayage(plan.de, 20.0, 20_000.0, 0.5, 3.0);
    let mut sortie = Vec::with_capacity(entree.len() * (plan.vers / plan.de + 1) as usize);
    sur.traiter(&entree, &mut sortie);
    let taille = 1 << 16;
    let p = dsp_welch(&sortie, taille);
    let df = plan.vers as f64 / taille as f64;
    let nyq = plan.de as f64 / 2.0;
    let (mut bande, mut images) = (0.0, 0.0);
    for (k, &v) in p.iter().enumerate() {
        let f = k as f64 * df;
        if f < nyq {
            bande += v;
        } else {
            images += v;
        }
    }
    let max = p.iter().cloned().fold(0.0, f64::max).max(1e-300);
    let spectre = p
        .iter()
        .enumerate()
        .map(|(k, &v)| (k as f64 * df, 10.0 * (v / max).max(1e-30).log10()))
        .collect();
    Images {
        rejet_db: -10.0 * (images / bande).max(1e-30).log10(),
        spectre,
    }
}

/// THD+N (dB, négatif) d'une sinusoïde à `f0_hz` (ramenée au centre d'un bin)
/// d'amplitude crête `a`, mesurée en sortie entre `f_min` et `f_max`, après
/// une éventuelle quantification. FFT rectangulaire de 2¹⁸ points sur une
/// sinusoïde exactement périodique ; le fondamental occupe `k0 ± 2`.
pub fn thd_n_db(
    sur: &mut Surechantillonneur,
    f0_hz: f64,
    a: f64,
    f_min: f64,
    f_max: f64,
    quant: Option<&mut Quantificateur>,
) -> f64 {
    assert_eq!(sur.canaux(), 1, "mesure mono");
    let plan = sur.plan();
    let n = 1 << 18;
    let f0 = (f0_hz * n as f64 / plan.vers as f64).round() * plan.vers as f64 / n as f64;
    let latence = sur.latence_theorique_trames_entree().ceil() as usize + 256;
    let trames_entree = (n + 1024) * plan.de as usize / plan.vers as usize + latence + 4096;
    let entree = sinus(plan.de, f0, a, trames_entree);
    let mut sortie = Vec::with_capacity(trames_entree * (plan.vers / plan.de + 1) as usize);
    sur.traiter(&entree, &mut sortie);
    if let Some(q) = quant {
        q.traiter(&mut sortie);
    }
    let debut = latence * plan.vers as usize / plan.de as usize;
    assert!(sortie.len() >= debut + n, "sortie trop courte : {}", sortie.len());
    // Pas de fenêtre : la sinusoïde est exactement périodique sur `n`
    // échantillons de sortie, la fuite spectrale est nulle. Une fenêtre
    // Blackman-Harris laisserait ses lobes (−92 dB, −6 dB/octave) dominer
    // le plancher f64 que l'on veut mesurer.
    let m = module_spectre(&sortie[debut..debut + n], n);
    let df = plan.vers as f64 / n as f64;
    let k0 = (f0 / df).round() as usize;
    let (mut fond, mut bruit) = (0.0, 0.0);
    for (k, &v) in m.iter().enumerate() {
        let f = k as f64 * df;
        if f < f_min || f > f_max || k < 4 {
            continue;
        }
        let p = v * v;
        if k.abs_diff(k0) <= 2 {
            fond += p;
        } else {
            bruit += p;
        }
    }
    10.0 * (bruit / fond).max(1e-40).log10()
}

/// Bruit de quantification mesuré sur une sinusoïde de 1 kHz à −20 dBFS,
/// directement à `cadence` (le filtre n'intervient pas).
#[derive(Clone, Debug)]
pub struct Bruit {
    /// Puissance totale de l'erreur, 0 → cadence/2, en dBFS.
    pub total_dbfs: f64,
    /// Puissance de l'erreur entre 20 Hz et 20 kHz, en dBFS.
    pub bande_audible_dbfs: f64,
    /// Même bande, pondérée par l'inverse du seuil d'audition (Terhardt).
    pub pondere_ath_db: f64,
    /// Densité spectrale de l'erreur `(Hz, dB)`, par bin de Welch 8 192.
    pub spectre: Vec<(f64, f64)>,
}

pub fn bruit_de_quantification(cadence: u32, quant: &mut Quantificateur, duree_s: f64) -> Bruit {
    let n = (duree_s * cadence as f64) as usize;
    let x = sinus(cadence, 1000.0, 0.1, n);
    let mut y = x.clone();
    quant.traiter(&mut y);
    let err: Vec<f64> = y.iter().zip(&x).map(|(a, b)| a - b).collect();
    let taille = 8192;
    let p = dsp_welch(&err, taille);
    let df = cadence as f64 / taille as f64;
    let (mut total, mut audible, mut pondere) = (0.0, 0.0, 0.0);
    for (k, &v) in p.iter().enumerate() {
        let f = k as f64 * df;
        total += v;
        if (20.0..=20_000.0).contains(&f) {
            audible += v;
            pondere += v * 10f64.powf(-ath_terhardt_db(f) / 10.0);
        }
    }
    let db = |v: f64| 10.0 * v.max(1e-40).log10();
    Bruit {
        total_dbfs: db(total),
        bande_audible_dbfs: db(audible),
        pondere_ath_db: db(pondere),
        spectre: p
            .iter()
            .enumerate()
            .map(|(k, &v)| (k as f64 * df, db(v)))
            .collect(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Latence {
    pub theorique_ms: f64,
    /// Instant du pic de la réponse à une impulsion.
    pub pic_ms: f64,
    /// Instant où la moitié de l'énergie de la réponse est passée.
    pub moitie_energie_ms: f64,
}

pub fn latence(sur: &mut Surechantillonneur) -> Latence {
    assert_eq!(sur.canaux(), 1, "mesure mono");
    let plan = sur.plan();
    let portee = sur.conception().portee + sur.portee_etage2() + 64;
    let mut entree = vec![0.0; portee];
    entree[0] = 1.0;
    let mut sortie = Vec::new();
    sur.traiter(&entree, &mut sortie);
    let e: Vec<f64> = sortie.iter().map(|v| v * v).collect();
    let total: f64 = e.iter().sum();
    let mut cumul = 0.0;
    let mut moitie = 0;
    for (i, v) in e.iter().enumerate() {
        cumul += v;
        if cumul >= total / 2.0 {
            moitie = i;
            break;
        }
    }
    let pic = sortie
        .iter()
        .enumerate()
        .fold((0, 0.0), |a, (i, &v)| if v.abs() > a.1 { (i, v.abs()) } else { a })
        .0;
    let ms = |i: usize| i as f64 * 1000.0 / plan.vers as f64;
    Latence {
        theorique_ms: sur.latence_theorique_ms(),
        pic_ms: ms(pic),
        moitie_energie_ms: ms(moitie),
    }
}

/// Amplitude crête (dBFS) d'une sinusoïde après passage, en régime établi.
pub fn gain_sinus_db(sur: &mut Surechantillonneur, f_hz: f64) -> f64 {
    assert_eq!(sur.canaux(), 1);
    let plan = sur.plan();
    let n = 8192 + 4 * sur.conception().portee;
    let entree = sinus(plan.de, f_hz, 0.5, n);
    let mut sortie = Vec::new();
    sur.traiter(&entree, &mut sortie);
    let saut = 2 * sur.conception().portee * plan.vers as usize / plan.de as usize;
    let crete = sortie[saut..].iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    20.0 * (crete / 0.5).log10()
}
