//! rc14 (Rémi) — la courbe d'un filtre de convolution, par FFT : amplitude
//! et phase, sur une grille logarithmique (≈ 512 points), pour l'écran TESS.
//!
//! # Calcul
//!
//! 1. La RI d'UN canal est complétée de zéros jusqu'à `n`, puissance de deux
//!    d'au moins deux fois sa longueur (la phase varie de moins de π d'une
//!    case à la suivante, condition du déroulement) et d'au moins `fs / 2`
//!    (une case ≤ 2 Hz : le grave reste lisible). Une seule FFT réelle.
//! 2. **Retard principal** `d` : la position du coefficient de plus grande
//!    valeur absolue (le pic). Il est retiré du spectre, exactement
//!    (`H[k]·e^{+j2πkd/n}`) : il reste la phase « hors retard », celle qui
//!    intéresse un filtre de correction de phase. Sans ce retrait, un FIR à
//!    phase linéaire de 64 k coefficients tourne de millions de degrés et la
//!    courbe ne se lit pas.
//! 3. **Déroulement** case par case sur la grille FINE de la FFT, puis lecture
//!    aux points logarithmiques par interpolation linéaire entre deux cases.
//!    Les cases dont l'amplitude est à plus de [`SEUIL_PHASE_DB`] sous le
//!    maximum n'ont pas de phase significative (bruit numérique d'une bande
//!    coupée) : elles ne font pas avancer le déroulement et leur phase est
//!    publiée absente (`None`), la courbe est alors interrompue.
//! 4. **Pas de lissage** : la phase publiée est la phase déroulée telle
//!    quelle. Le retrait du retard et le seuil suffisent à la rendre lisible
//!    sur les filtres de correction ; un lissage fractionnaire d'octave
//!    masquerait justement les rotations qu'on cherche à voir.
//!
//! Calcul fait une fois par filtre (l'appelant le met en cache), hors du fil
//! temps réel.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Nombre de points de la grille logarithmique publiée.
pub const POINTS_DE_LA_GRILLE: usize = 512;
/// Sous ce niveau (relatif au maximum de l'amplitude), la phase n'est pas
/// publiée : elle n'a pas de sens.
pub const SEUIL_PHASE_DB: f64 = -60.0;
/// Bornes de la grille : 20 Hz, et 20 kHz au plus (0,45·fs sinon).
pub const F_MIN_HZ: f64 = 20.0;
pub const F_MAX_HZ: f64 = 20_000.0;
/// Taille maximale de la FFT (au-delà, la RI elle-même impose sa taille).
const N_MAX: usize = 1 << 22;

/// La courbe d'un canal.
#[derive(Debug, Clone, PartialEq)]
pub struct CourbeCanal {
    pub freqs_hz: Vec<f64>,
    /// 20·log10|H|, plancher −200 dB.
    pub magnitude_db: Vec<f64>,
    /// Phase principale ([−180°, 180°]), retard compris (ancienne forme).
    pub phase_deg: Vec<f64>,
    /// Phase DÉROULÉE, retard principal retiré ; `None` sous le seuil.
    pub phase_deroulee_deg: Vec<Option<f64>>,
    /// Le retard principal retiré, en échantillons.
    pub retard_echantillons: usize,
}

impl CourbeCanal {
    /// La phase déroulée AVEC le retard : `φ(f) − 360·f·d/fs`.
    pub fn phase_deroulee_brute_deg(&self, sample_rate: u32) -> Vec<Option<f64>> {
        let d = self.retard_echantillons as f64;
        self.freqs_hz
            .iter()
            .zip(&self.phase_deroulee_deg)
            .map(|(f, p)| p.map(|p| p - 360.0 * f * d / f64::from(sample_rate)))
            .collect()
    }
}

/// La grille logarithmique publiée pour cette cadence.
pub fn grille(sample_rate: u32, points: usize) -> Vec<f64> {
    let haut = F_MAX_HZ.min(f64::from(sample_rate) * 0.45);
    super::log_freq_grid(points, F_MIN_HZ, haut)
}

fn taille_fft(longueur: usize, sample_rate: u32) -> usize {
    let voulu = (2 * longueur.max(1))
        .max(sample_rate as usize / 2)
        .max(4096);
    let n = voulu.next_power_of_two();
    if n > N_MAX && 2 * longueur <= N_MAX {
        N_MAX
    } else {
        n
    }
}

fn replier_deg(x: f64) -> f64 {
    let r = (x + 180.0).rem_euclid(360.0) - 180.0;
    if r == -180.0 { 180.0 } else { r }
}

/// Le spectre d'un canal sur la grille FINE de la FFT : `(n, H[0..=n/2])`,
/// `n` choisi par [`taille_fft`]. Une seule FFT réelle, hors temps réel.
fn spectre_du_canal(taps: &[f32], sample_rate: u32) -> Option<(usize, Vec<Complex<f64>>)> {
    if taps.is_empty() || sample_rate == 0 {
        return None;
    }
    let n = taille_fft(taps.len(), sample_rate);
    let mut planificateur = RealFftPlanner::<f64>::new();
    let fft = planificateur.plan_fft_forward(n);
    let mut entree = fft.make_input_vec();
    for (e, t) in entree.iter_mut().zip(taps) {
        *e = f64::from(*t);
    }
    let mut spectre = fft.make_output_vec();
    fft.process(&mut entree, &mut spectre).ok()?;
    Some((n, spectre))
}

/// rc14 (Didier) — le gain MAXIMAL de la réponse en fréquence d'un filtre,
/// `max |H(f)|` en dB, pris au PIRE canal, de 0 Hz à Nyquist, sur la grille
/// fine de la FFT (une case ≤ 2 Hz, voir [`taille_fft`]) : c'est ce dont un
/// sinus pleine échelle peut monter à la traversée du filtre.
///
/// La même FFT que la courbe ([`courbe_du_canal`]) ; calculée UNE fois par
/// filtre et par cadence (l'appelant la met en cache), jamais dans le fil
/// audio. `None` pour une RI vide, une cadence nulle ou un spectre nul.
pub fn gain_max_db(ri: &[Vec<f32>], sample_rate: u32) -> Option<f64> {
    let mut pire: Option<f64> = None;
    for canal in ri {
        let (_, spectre) = spectre_du_canal(canal, sample_rate)?;
        let max = spectre.iter().map(|h| h.norm()).fold(0.0f64, f64::max);
        if max > 0.0 && max.is_finite() {
            let db = 20.0 * max.log10();
            pire = Some(pire.map_or(db, |p: f64| p.max(db)));
        }
    }
    pire
}

/// La courbe d'un canal (`taps`) à la cadence `sample_rate`, sur `points`
/// points logarithmiques. `None` pour une RI vide ou une cadence nulle.
pub fn courbe_du_canal(taps: &[f32], sample_rate: u32, points: usize) -> Option<CourbeCanal> {
    if taps.is_empty() || sample_rate == 0 {
        return None;
    }
    let fs = f64::from(sample_rate);
    let retard = taps
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let (n, mut spectre) = spectre_du_canal(taps, sample_rate)?;

    // Retrait exact du retard principal.
    let pas = 2.0 * std::f64::consts::PI * retard as f64 / n as f64;
    for (k, h) in spectre.iter_mut().enumerate() {
        let (s, c) = (pas * k as f64).sin_cos();
        *h *= Complex::new(c, s);
    }

    // Amplitude de chaque case, et seuil de la phase.
    let module: Vec<f64> = spectre.iter().map(|h| h.norm()).collect();
    let max = module.iter().copied().fold(0.0f64, f64::max);
    let seuil = max * 10f64.powf(SEUIL_PHASE_DB / 20.0);

    // Déroulement sur la grille fine, en sautant les cases sous le seuil.
    let mut deroulee: Vec<Option<f64>> = vec![None; spectre.len()];
    let mut precedente: Option<f64> = None; // phase principale de la dernière case valide
    let mut cumul = 0.0f64;
    for (k, h) in spectre.iter().enumerate() {
        if module[k] <= seuil || module[k] == 0.0 {
            continue;
        }
        let p = h.arg();
        if let Some(q) = precedente {
            let mut d = p - q;
            while d > std::f64::consts::PI {
                d -= 2.0 * std::f64::consts::PI;
            }
            while d < -std::f64::consts::PI {
                d += 2.0 * std::f64::consts::PI;
            }
            cumul += d;
        } else {
            cumul = p;
        }
        precedente = Some(p);
        deroulee[k] = Some(cumul);
    }

    let freqs = grille(sample_rate, points);
    let derniere = spectre.len() - 1;
    let mut magnitude_db = Vec::with_capacity(freqs.len());
    let mut phase_deg = Vec::with_capacity(freqs.len());
    let mut phase_deroulee_deg = Vec::with_capacity(freqs.len());
    for &f in &freqs {
        let x = (f * n as f64 / fs).clamp(0.0, derniere as f64);
        let k0 = (x.floor() as usize).min(derniere);
        let k1 = (k0 + 1).min(derniere);
        let t = x - k0 as f64;
        let h = spectre[k0] * (1.0 - t) + spectre[k1] * t;
        let m = h.norm();
        magnitude_db.push(if m > 1e-10 { 20.0 * m.log10() } else { -200.0 });
        let ex = match (deroulee[k0], deroulee[k1]) {
            (Some(a), Some(b)) => Some(a + (b - a) * t),
            (Some(a), None) if t == 0.0 => Some(a),
            _ => None,
        };
        let ex_deg = ex.map(f64::to_degrees);
        // La phase principale garde le retard (forme de l'ancienne réponse).
        let lineaire = 360.0 * f * retard as f64 / fs;
        phase_deg.push(replier_deg(h.arg().to_degrees() - lineaire));
        phase_deroulee_deg.push(ex_deg);
    }
    Some(CourbeCanal {
        freqs_hz: freqs,
        magnitude_db,
        phase_deg,
        phase_deroulee_deg,
        retard_echantillons: retard,
    })
}

#[cfg(test)]
#[path = "courbe_fft_tests.rs"]
mod tests;
