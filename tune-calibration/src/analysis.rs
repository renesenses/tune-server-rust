//! Analyse d'une réponse impulsionnelle : fenêtrage, réponse en fréquence,
//! lissage en fraction d'octave, grille logarithmique, RT60 par bande
//! (intégration rétrograde de Schroeder, ISO 3382-1), cascade, écart
//! gauche/droite et moyenne spatiale.
//!
//! Références : M. R. Schroeder, « New Method of Measuring Reverberation
//! Time », JASA 37, 1965 ; ISO 3382-1:2009 (EDT, T20, T30) ; lissage en
//! fraction d'octave tel que pratiqué par REW (moyenne des dB sur une
//! fenêtre de largeur 1/N octave centrée sur chaque fréquence).

use crate::deconv::ImpulseResponse;
use crate::fft;
use realfft::num_complex::Complex;

/// Réponse en fréquence sur grille linéaire (sortie directe de la FFT).
/// `phase_rad` et `complex` peuvent être vides quand seule la magnitude compte.
#[derive(Debug, Clone)]
pub struct FrequencyResponse {
    pub freqs: Vec<f64>,
    pub magnitude_db: Vec<f64>,
    pub phase_rad: Vec<f64>,
    pub complex: Vec<Complex<f64>>,
}

/// Réponse en magnitude sur grille logarithmique (support de l'égaliseur).
#[derive(Debug, Clone, PartialEq)]
pub struct LogResponse {
    pub freqs: Vec<f64>,
    pub magnitude_db: Vec<f64>,
}

impl LogResponse {
    /// Grille log : `points_per_octave` points par octave de `f_lo` à `f_hi` inclus.
    pub fn grid(f_lo: f64, f_hi: f64, points_per_octave: usize) -> Vec<f64> {
        let octaves = (f_hi / f_lo).log2();
        let n = (octaves * points_per_octave as f64).ceil() as usize + 1;
        (0..n)
            .map(|i| f_lo * 2f64.powf(i as f64 / points_per_octave as f64))
            .filter(|f| *f <= f_hi * 1.0000001)
            .collect()
    }

    /// Interpolation log-linéaire de la magnitude (dB) à la fréquence `f`.
    pub fn at(&self, f: f64) -> f64 {
        interp_log(&self.freqs, &self.magnitude_db, f)
    }

    /// Lissage 1/N d'octave sur la grille log elle-même (moyenne des dB sur
    /// ±1/2N octave). Sert à l'égaliseur, qui travaille sur une courbe plus
    /// lisse que celle affichée.
    pub fn smoothed(&self, n: usize) -> LogResponse {
        let half = 2f64.powf(0.5 / n as f64);
        let mags = (0..self.freqs.len())
            .map(|i| {
                let f = self.freqs[i];
                let (lo, hi) = (f / half, f * half);
                let mut acc = 0.0;
                let mut cnt = 0usize;
                for (g, m) in self.freqs.iter().zip(&self.magnitude_db) {
                    if *g >= lo && *g <= hi {
                        acc += m;
                        cnt += 1;
                    }
                }
                acc / cnt.max(1) as f64
            })
            .collect();
        LogResponse {
            freqs: self.freqs.clone(),
            magnitude_db: mags,
        }
    }

    /// Valeur moyenne (dB) entre deux fréquences.
    pub fn mean_db(&self, f_lo: f64, f_hi: f64) -> f64 {
        let vals: Vec<f64> = self
            .freqs
            .iter()
            .zip(&self.magnitude_db)
            .filter(|(f, _)| **f >= f_lo && **f <= f_hi)
            .map(|(_, m)| *m)
            .collect();
        if vals.is_empty() {
            return 0.0;
        }
        vals.iter().sum::<f64>() / vals.len() as f64
    }
}

/// Interpolation linéaire en log(f) d'une table (f croissants), bornée aux extrémités.
pub fn interp_log(freqs: &[f64], values: &[f64], f: f64) -> f64 {
    if freqs.is_empty() {
        return 0.0;
    }
    if f <= freqs[0] {
        return values[0];
    }
    if f >= freqs[freqs.len() - 1] {
        return values[values.len() - 1];
    }
    let i = match freqs.binary_search_by(|x| x.partial_cmp(&f).unwrap()) {
        Ok(i) => return values[i],
        Err(i) => i,
    };
    let (f0, f1) = (freqs[i - 1], freqs[i]);
    let t = (f / f0).ln() / (f1 / f0).ln();
    values[i - 1] + t * (values[i] - values[i - 1])
}

/// Fenêtre d'analyse autour de l'arrivée directe : demi-Hann montante de
/// `pre_ms` avant le pic, plateau, demi-Hann descendante sur les derniers
/// `taper_ms` de la fenêtre `post_ms`. Renvoie le segment fenêtré, le pic
/// au rang `pre` (ou moins si l'IR commence plus tard).
pub fn window_ir(ir: &ImpulseResponse, pre_ms: f64, post_ms: f64, taper_ms: f64) -> Vec<f64> {
    let fs = ir.sample_rate as f64;
    let pre = ((pre_ms / 1000.0) * fs).round() as usize;
    let post = ((post_ms / 1000.0) * fs).round() as usize;
    let taper = (((taper_ms / 1000.0) * fs).round() as usize).min(post);
    let start = ir.peak_index.saturating_sub(pre);
    let pre_eff = ir.peak_index - start;
    let end = (ir.peak_index + post).min(ir.samples.len());
    let mut out = Vec::with_capacity(end - start);
    for (j, &v) in ir.samples[start..end].iter().enumerate() {
        let w = if j < pre_eff {
            0.5 - 0.5 * (std::f64::consts::PI * j as f64 / pre_eff.max(1) as f64).cos()
        } else {
            let k = j - pre_eff;
            if k >= post.saturating_sub(taper) && taper > 0 {
                let m = k - (post - taper);
                0.5 + 0.5 * (std::f64::consts::PI * m as f64 / taper as f64).cos()
            } else {
                1.0
            }
        };
        out.push(v * w);
    }
    out
}

/// Réponse en fréquence d'un segment (FFT de taille `nfft` ≥ segment, puissance de 2).
pub fn frequency_response(segment: &[f64], sample_rate: u32, nfft: usize) -> FrequencyResponse {
    let nfft = fft::next_pow2(nfft.max(segment.len()));
    let spec = fft::rfft(segment, nfft);
    let df = sample_rate as f64 / nfft as f64;
    let freqs: Vec<f64> = (0..spec.len()).map(|i| i as f64 * df).collect();
    let magnitude_db: Vec<f64> = spec.iter().map(|c| fft::to_db(c.norm())).collect();
    let phase_rad: Vec<f64> = spec.iter().map(|c| c.arg()).collect();
    FrequencyResponse {
        freqs,
        magnitude_db,
        phase_rad,
        complex: spec,
    }
}

/// Lissage en fraction d'octave (1/N) : moyenne des dB sur [f·2^(−1/2N), f·2^(1/2N)].
/// Renvoie une nouvelle réponse sur la même grille linéaire.
pub fn smooth_fractional_octave(fr: &FrequencyResponse, n: usize) -> Vec<f64> {
    let half = 2f64.powf(0.5 / n as f64);
    let len = fr.freqs.len();
    let mut out = vec![0.0; len];
    let mut lo = 0usize;
    let mut hi = 0usize;
    for i in 0..len {
        let f = fr.freqs[i];
        if f <= 0.0 {
            out[i] = fr.magnitude_db[i];
            continue;
        }
        let f_lo = f / half;
        let f_hi = f * half;
        while lo < len && fr.freqs[lo] < f_lo {
            lo += 1;
        }
        if hi < lo {
            hi = lo;
        }
        while hi < len && fr.freqs[hi] <= f_hi {
            hi += 1;
        }
        let count = (hi - lo).max(1);
        let sum: f64 = fr.magnitude_db[lo..hi.max(lo + 1).min(len)].iter().sum();
        out[i] = sum / count as f64;
    }
    out
}

/// Rééchantillonne une réponse lissée (grille linéaire) sur une grille log.
pub fn to_log_grid(freqs: &[f64], magnitude_db: &[f64], grid: &[f64]) -> LogResponse {
    let mags = grid
        .iter()
        .map(|f| interp_log(freqs, magnitude_db, *f))
        .collect();
    LogResponse {
        freqs: grid.to_vec(),
        magnitude_db: mags,
    }
}

/// Enchaîne fenêtrage → FFT → lissage 1/N → grille log. Fenêtre gauche de
/// 100 ms : elle garde la pré-oscillation basse fréquence du noyau de
/// déconvolution (bord du balayage à 10–20 Hz, ~100 ms), qu'une fenêtre de
/// 5 ms tronquait au prix de 0,4 dB d'erreur entre deux modes et 40 ms au
/// prix de 0,35 dB à 30 Hz ; REW garde 125 ms par défaut.
pub fn measured_response(
    ir: &ImpulseResponse,
    window_post_ms: f64,
    smoothing_n: usize,
    grid: &[f64],
) -> LogResponse {
    let seg = window_ir(ir, 100.0, window_post_ms, window_post_ms * 0.5);
    let nfft = fft::next_pow2(seg.len().max(ir.sample_rate as usize));
    let fr = frequency_response(&seg, ir.sample_rate, nfft);
    let sm = smooth_fractional_octave(&fr, smoothing_n);
    to_log_grid(&fr.freqs, &sm, grid)
}

/// Écart gauche − droite (dB) point à point sur une même grille.
pub fn difference_db(left: &LogResponse, right: &LogResponse) -> LogResponse {
    assert_eq!(left.freqs.len(), right.freqs.len());
    LogResponse {
        freqs: left.freqs.clone(),
        magnitude_db: left
            .magnitude_db
            .iter()
            .zip(&right.magnitude_db)
            .map(|(a, b)| a - b)
            .collect(),
    }
}

/// Moyenne spatiale pondérée de plusieurs positions. `power = true` moyenne
/// les puissances (comme REW par défaut) ; sinon les dB.
pub fn spatial_average(responses: &[LogResponse], weights: &[f64], power: bool) -> LogResponse {
    assert!(!responses.is_empty() && responses.len() == weights.len());
    let n = responses[0].freqs.len();
    let wsum: f64 = weights.iter().sum();
    let mut out = vec![0.0; n];
    for i in 0..n {
        let mut acc = 0.0;
        for (r, w) in responses.iter().zip(weights) {
            let m = r.magnitude_db[i];
            acc += w * if power { 10f64.powf(m / 10.0) } else { m };
        }
        acc /= wsum;
        out[i] = if power { 10.0 * acc.log10() } else { acc };
    }
    LogResponse {
        freqs: responses[0].freqs.clone(),
        magnitude_db: out,
    }
}

/// Résultat du temps de réverbération d'une bande.
#[derive(Debug, Clone, PartialEq)]
pub struct BandRt60 {
    pub center_hz: f64,
    /// Early Decay Time : pente 0 → −10 dB, ×6.
    pub edt_s: f64,
    /// Pente −5 → −25 dB, ×3.
    pub t20_s: f64,
    /// Pente −5 → −35 dB, ×2 (NaN si la dynamique manque).
    pub t30_s: f64,
    /// Dynamique disponible (dB) entre le pic de la courbe de décroissance et le bruit.
    pub dynamic_range_db: f64,
}

/// Filtre passe-bande à phase nulle dans le domaine fréquentiel (fenêtre en
/// cosinus de 1/6 d'octave sur les flancs). Suffisant pour le prototype ;
/// l'implémentation serveur prendra des Butterworth d'ordre 6 (CEI 61260).
pub fn bandpass_zero_phase(x: &[f64], sample_rate: u32, f_lo: f64, f_hi: f64) -> Vec<f64> {
    let nfft = fft::next_pow2(x.len() * 2);
    let mut spec = fft::rfft(x, nfft);
    let df = sample_rate as f64 / nfft as f64;
    let edge = 2f64.powf(1.0 / 12.0);
    for (i, c) in spec.iter_mut().enumerate() {
        let f = i as f64 * df;
        let w = if f < f_lo / edge || f > f_hi * edge {
            0.0
        } else if f < f_lo {
            let t = (f / (f_lo / edge)).ln() / edge.ln();
            0.5 - 0.5 * (std::f64::consts::PI * t).cos()
        } else if f > f_hi {
            let t = (f / f_hi).ln() / edge.ln();
            0.5 + 0.5 * (std::f64::consts::PI * t).cos()
        } else {
            1.0
        };
        *c *= w;
    }
    let mut y = fft::irfft(&spec, nfft);
    y.truncate(x.len());
    y
}

/// Courbe de décroissance de Schroeder (dB, max à 0) d'un segment, avec
/// soustraction du bruit estimé sur les derniers 10 % (méthode de Chu).
pub fn schroeder_decay_db(h: &[f64]) -> Vec<f64> {
    let n = h.len();
    let tail_start = n - n / 10;
    let noise = h[tail_start..].iter().map(|v| v * v).sum::<f64>() / (n - tail_start).max(1) as f64;
    let mut acc = 0.0;
    let mut e = vec![0.0; n];
    for i in (0..n).rev() {
        acc += (h[i] * h[i] - noise).max(0.0);
        e[i] = acc;
    }
    let e0 = e[0].max(1e-30);
    e.iter()
        .map(|v| 10.0 * (v / e0).max(1e-30).log10())
        .collect()
}

/// Régression linéaire de la courbe `decay_db` entre deux niveaux (dB,
/// négatifs) ; renvoie le temps (s) de chute de 60 dB extrapolé.
fn decay_time(decay_db: &[f64], sample_rate: u32, from_db: f64, to_db: f64) -> f64 {
    let i0 = decay_db.iter().position(|v| *v <= from_db);
    let i1 = decay_db.iter().position(|v| *v <= to_db);
    let (Some(i0), Some(i1)) = (i0, i1) else {
        return f64::NAN;
    };
    if i1 <= i0 + 2 {
        return f64::NAN;
    }
    let fs = sample_rate as f64;
    let n = (i1 - i0) as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for i in i0..i1 {
        let x = i as f64 / fs;
        let y = decay_db[i];
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    if slope >= 0.0 {
        return f64::NAN;
    }
    -60.0 / slope
}

/// RT60 (EDT, T20, T30) par bande d'octave ou de tiers d'octave.
/// `centers` : fréquences centrales ; `bandwidth_octaves` : 1.0 ou 1/3.
pub fn rt60_by_band(
    ir: &ImpulseResponse,
    centers: &[f64],
    bandwidth_octaves: f64,
) -> Vec<BandRt60> {
    // On part un peu avant l'arrivée directe pour inclure le pré-écho du filtrage.
    let fs = ir.sample_rate as f64;
    let start = ir.peak_index.saturating_sub((0.005 * fs) as usize);
    let h = &ir.samples[start..];
    let half = 2f64.powf(bandwidth_octaves / 2.0);
    centers
        .iter()
        .map(|&fc| {
            let band = bandpass_zero_phase(h, ir.sample_rate, fc / half, fc * half);
            let decay = schroeder_decay_db(&band);
            let dynamic =
                -decay
                    .iter()
                    .cloned()
                    .fold(0.0f64, |m, v| if v.is_finite() { m.min(v) } else { m });
            BandRt60 {
                center_hz: fc,
                edt_s: decay_time(&decay, ir.sample_rate, 0.0, -10.0),
                t20_s: decay_time(&decay, ir.sample_rate, -5.0, -25.0),
                t30_s: decay_time(&decay, ir.sample_rate, -5.0, -35.0),
                dynamic_range_db: dynamic,
            }
        })
        .collect()
}

/// Fréquences centrales d'octave normalisées (CEI 61260) de 31,5 Hz à 16 kHz.
pub const OCTAVE_CENTERS: [f64; 10] = [
    31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// Cascade (CSD / « waterfall ») : spectres successifs de fenêtres glissantes
/// de durée `window_ms` dont le début avance de `step_ms`, sur `slices` tranches.
#[derive(Debug, Clone)]
pub struct Waterfall {
    pub times_ms: Vec<f64>,
    pub freqs: Vec<f64>,
    /// `magnitudes_db[tranche][fréquence]`, en dB relatifs au maximum de la première tranche.
    pub magnitudes_db: Vec<Vec<f64>>,
}

pub fn waterfall(
    ir: &ImpulseResponse,
    window_ms: f64,
    step_ms: f64,
    slices: usize,
    grid: &[f64],
) -> Waterfall {
    let fs = ir.sample_rate as f64;
    let win = ((window_ms / 1000.0) * fs).round() as usize;
    let step = ((step_ms / 1000.0) * fs).round().max(1.0) as usize;
    let nfft = fft::next_pow2(win.max(4096));
    let mut times = Vec::with_capacity(slices);
    let mut mags = Vec::with_capacity(slices);
    let mut reference = f64::NEG_INFINITY;
    for s in 0..slices {
        let start = ir.peak_index + s * step;
        if start >= ir.samples.len() {
            break;
        }
        let end = (start + win).min(ir.samples.len());
        // Fenêtre de Hann asymétrique : montée raide, descente sur toute la fenêtre.
        let seg: Vec<f64> = ir.samples[start..end]
            .iter()
            .enumerate()
            .map(|(j, v)| {
                let w = 0.5 + 0.5 * (std::f64::consts::PI * j as f64 / win as f64).cos();
                v * w
            })
            .collect();
        let fr = frequency_response(&seg, ir.sample_rate, nfft);
        let sm = smooth_fractional_octave(&fr, 6);
        let lr = to_log_grid(&fr.freqs, &sm, grid);
        if s == 0 {
            reference = lr
                .magnitude_db
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max);
        }
        times.push(s as f64 * step_ms);
        mags.push(lr.magnitude_db.iter().map(|m| m - reference).collect());
    }
    Waterfall {
        times_ms: times,
        freqs: grid.to_vec(),
        magnitudes_db: mags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grille_log_bornes() {
        let g = LogResponse::grid(20.0, 20_000.0, 12);
        assert!((g[0] - 20.0).abs() < 1e-9);
        assert!(g.last().unwrap() <= &20_000.0001);
        assert_eq!(g.len(), 120); // 20·2^(120/12) = 20 480 > 20 000 est exclu
    }

    #[test]
    fn interpolation_log() {
        let f = [100.0, 1000.0];
        let v = [0.0, 20.0];
        assert!((interp_log(&f, &v, 316.227766) - 10.0).abs() < 1e-6);
        assert_eq!(interp_log(&f, &v, 10.0), 0.0);
        assert_eq!(interp_log(&f, &v, 1e5), 20.0);
    }

    #[test]
    fn decroissance_exponentielle_donne_le_bon_rt60() {
        let fs = 48_000u32;
        let rt = 0.6;
        let n = (1.5 * fs as f64) as usize;
        let mut state = 12345u64;
        let h: Vec<f64> = (0..n)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let u = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
                let t = i as f64 / fs as f64;
                u * 10f64.powf(-3.0 * t / rt)
            })
            .collect();
        let d = schroeder_decay_db(&h);
        let t30 = decay_time(&d, fs, -5.0, -35.0);
        assert!((t30 - rt).abs() / rt < 0.05, "T30 {t30} ≠ {rt}");
    }
}
