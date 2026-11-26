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
    /// Pente −5 → −35 dB, ×2.
    ///
    /// Chacun des trois temps vaut NaN quand la plage dynamique de la bande
    /// ne couvre pas sa plage d'évaluation avec 10 dB de marge : la bande
    /// est alors « non mesurable », pas mesurée à une valeur fausse.
    pub t30_s: f64,
    /// Plage dynamique de la bande (dB) : crête de l'enveloppe d'énergie
    /// au-dessus du bruit de fond. Toujours finie.
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

/// Courbe de décroissance de Schroeder (dB, max à 0) d'un segment, intégrée
/// jusqu'au bout, avec soustraction du bruit estimé sur les derniers 10 %
/// (méthode de Chu).
///
/// ⚠️ Sans troncature, cette courbe n'est fiable qu'avec un bruit de fond
/// très bas : dès que le bruit dépasse quelques pour cent de l'énergie de la
/// décroissance, sa part intégrée aplatit la courbe et la pente donne des
/// RT60 de plusieurs secondes (voir les tests). [`rt60_by_band`] emploie
/// [`schroeder_truncated`].
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

/// Bruit de fond et point de croisement d'une décroissance, estimés par la
/// méthode itérative de Lundeby.
///
/// Réf. : A. Lundeby, T. E. Vigran, H. Bietz, M. Vorländer, « Uncertainties
/// of Measurements in Room Acoustics », Acustica 81, 1995 ; ISO 3382-1:2009,
/// annexe sur la troncature de l'intégrale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseCrossing {
    /// Puissance moyenne du bruit de fond, par échantillon (unités de h²).
    pub noise_power: f64,
    /// Rang où la droite de décroissance rejoint le bruit de fond.
    pub crossing: usize,
    /// Pente tardive de la décroissance (dB par échantillon, négative).
    pub slope_db_per_sample: f64,
    /// Écart (dB) entre la crête de l'enveloppe d'énergie et le bruit de fond.
    pub peak_to_noise_db: f64,
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn power_db(p: f64) -> f64 {
    10.0 * p.max(1e-300).log10()
}

/// Enveloppe d'énergie moyennée par blocs de `w` échantillons : (rang du
/// milieu du bloc, niveau dB).
fn block_envelope_db(e: &[f64], w: usize) -> Vec<(f64, f64)> {
    let w = w.max(1);
    e.chunks(w)
        .enumerate()
        .map(|(k, c)| ((k * w) as f64 + c.len() as f64 / 2.0, power_db(mean(c))))
        .collect()
}

/// Droite des moindres carrés (pente, ordonnée à l'origine).
fn linear_fit(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    if pts.len() < 2 {
        return None;
    }
    let n = pts.len() as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for &(x, y) in pts {
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let d = n * sxx - sx * sx;
    if d.abs() < 1e-30 {
        return None;
    }
    let slope = (n * sxy - sx * sy) / d;
    Some((slope, (sy - slope * sx) / n))
}

/// Points de l'enveloppe, à partir de la crête, compris entre `hi` (inclus)
/// et `lo` (exclu, premier point sous `lo` = fin de la plage).
fn decay_points(env: &[(f64, f64)], from: usize, hi: f64, lo: f64) -> Vec<(f64, f64)> {
    env[from..]
        .iter()
        .copied()
        .skip_while(|&(_, l)| l > hi)
        .take_while(|&(_, l)| l >= lo)
        .collect()
}

/// Bruit de fond et point de croisement de l'énergie `e` (= h²) d'une bande.
/// `None` quand la décroissance ne dépasse pas le bruit d'au moins 10 dB :
/// aucune pente n'est alors mesurable.
pub fn lundeby(e: &[f64], sample_rate: u32) -> Option<NoiseCrossing> {
    let fs = sample_rate as f64;
    let n = e.len();
    if n < (0.05 * fs) as usize {
        return None;
    }
    let tail = n - n / 10;
    // 1. Enveloppe sur 10 ms, bruit sur les derniers 10 %.
    let mut noise = mean(&e[tail..]);
    let mut noise_db = power_db(noise);
    let mut w = ((0.010 * fs) as usize).max(1);
    let mut env = block_envelope_db(e, w);
    let crete = |env: &[(f64, f64)]| {
        env.iter()
            .enumerate()
            .fold((0usize, f64::NEG_INFINITY), |m, (i, &(_, l))| {
                if l > m.1 {
                    (i, l)
                } else {
                    m
                }
            })
    };
    let (mut imax, mut lmax) = crete(&env);
    // 2. Première pente : de la crête jusqu'à 10 dB au-dessus du bruit.
    let (mut slope, b) = linear_fit(&decay_points(&env, imax, lmax, noise_db + 10.0))?;
    if slope >= 0.0 {
        return None;
    }
    let mut tc = (noise_db - b) / slope;
    // 3. Itérations : intervalle adapté à la pente (5 par 10 dB), bruit
    //    relu 10 dB de décroissance après le croisement (au moins les
    //    derniers 10 %), pente tardive entre bruit + 25 dB et bruit + 5 dB.
    for _ in 0..5 {
        let par_10_db = 10.0 / -slope;
        w = ((par_10_db / 5.0) as usize).clamp((0.002 * fs) as usize, (0.050 * fs) as usize);
        env = block_envelope_db(e, w);
        (imax, lmax) = crete(&env);
        let debut_bruit = ((tc + par_10_db).max(0.0) as usize).min(tail);
        noise = mean(&e[debut_bruit..]);
        noise_db = power_db(noise);
        let hi = (noise_db + 25.0).min(lmax);
        let Some((s, b)) = linear_fit(&decay_points(&env, imax, hi, noise_db + 5.0)) else {
            break;
        };
        if s >= 0.0 {
            break;
        }
        let tc_new = (noise_db - b) / s;
        slope = s;
        let converge = (tc_new - tc).abs() < w as f64;
        tc = tc_new;
        if converge {
            break;
        }
    }
    let debut = env[imax].0 as usize;
    Some(NoiseCrossing {
        noise_power: noise,
        crossing: (tc.max(0.0) as usize).clamp(debut.min(n), n),
        slope_db_per_sample: slope,
        peak_to_noise_db: lmax - noise_db,
    })
}

/// Courbe de décroissance de Schroeder tronquée au point de croisement avec
/// le bruit (Lundeby), le bruit soustrait avant le croisement et l'énergie
/// manquante au-delà compensée par la pente tardive. La courbe s'arrête au
/// croisement : au-delà, elle ne décrirait que le bruit.
pub fn schroeder_truncated(h: &[f64], sample_rate: u32) -> Option<(Vec<f64>, NoiseCrossing)> {
    let e: Vec<f64> = h.iter().map(|v| v * v).collect();
    let nc = lundeby(&e, sample_rate)?;
    if nc.crossing < 2 {
        return None;
    }
    // Énergie de la décroissance prolongée au-delà du croisement, où elle
    // vaut le bruit : ∫ P·10^(s·k/10) dk = P·10 / (−s·ln 10).
    let compensation = nc.noise_power * 10.0 / (-nc.slope_db_per_sample * std::f64::consts::LN_10);
    let mut acc = compensation;
    let mut s = vec![0.0; nc.crossing];
    for i in (0..nc.crossing).rev() {
        acc += e[i] - nc.noise_power;
        s[i] = acc.max(1e-300);
    }
    let s0 = s[0];
    Some((s.iter().map(|v| 10.0 * (v / s0).log10()).collect(), nc))
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

/// Marge exigée entre le bas de la plage d'évaluation et le bruit de fond
/// (ISO 3382-1 : au moins 10 dB). EDT (0 → −10 dB) demande donc 20 dB de
/// crête au-dessus du bruit, T20 (−5 → −25 dB) 35 dB, T30 (−5 → −35 dB) 45 dB.
pub const NOISE_MARGIN_DB: f64 = 10.0;

/// Le temps tiré de la plage `from_db → to_db`, ou NaN si la plage
/// dynamique de la bande ne la couvre pas avec la marge.
fn decay_time_if_measurable(
    decay_db: &[f64],
    sample_rate: u32,
    from_db: f64,
    to_db: f64,
    peak_to_noise_db: f64,
) -> f64 {
    if peak_to_noise_db < -to_db + NOISE_MARGIN_DB {
        return f64::NAN;
    }
    decay_time(decay_db, sample_rate, from_db, to_db)
}

/// RT60 (EDT, T20, T30) par bande d'octave ou de tiers d'octave.
/// `centers` : fréquences centrales ; `bandwidth_octaves` : 1.0 ou 1/3.
///
/// L'intégrale de Schroeder est tronquée au croisement avec le bruit de
/// fond (Lundeby). Chaque grandeur n'est rendue que si la plage dynamique de
/// la bande la permet ([`NOISE_MARGIN_DB`]) ; sinon NaN — « non mesurable »,
/// jamais une valeur extrapolée du bruit.
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
            let Some((decay, nc)) = schroeder_truncated(&band, ir.sample_rate) else {
                return BandRt60 {
                    center_hz: fc,
                    edt_s: f64::NAN,
                    t20_s: f64::NAN,
                    t30_s: f64::NAN,
                    dynamic_range_db: peak_to_noise_db(&band, ir.sample_rate),
                };
            };
            let d = nc.peak_to_noise_db;
            BandRt60 {
                center_hz: fc,
                edt_s: decay_time_if_measurable(&decay, ir.sample_rate, 0.0, -10.0, d),
                t20_s: decay_time_if_measurable(&decay, ir.sample_rate, -5.0, -25.0, d),
                t30_s: decay_time_if_measurable(&decay, ir.sample_rate, -5.0, -35.0, d),
                dynamic_range_db: d,
            }
        })
        .collect()
}

/// Crête de l'enveloppe (10 ms) au-dessus du bruit des derniers 10 %, quand
/// Lundeby n'a pas trouvé de décroissance. Toujours finie (≥ 0).
fn peak_to_noise_db(h: &[f64], sample_rate: u32) -> f64 {
    let e: Vec<f64> = h.iter().map(|v| v * v).collect();
    if e.is_empty() {
        return 0.0;
    }
    let tail = e.len() - e.len() / 10;
    let noise_db = power_db(mean(&e[tail..]));
    let w = ((0.010 * sample_rate as f64) as usize).max(1);
    let lmax = block_envelope_db(&e, w)
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max);
    (lmax - noise_db).max(0.0)
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

    /// Réponse impulsionnelle synthétique : 0,15 s avant l'arrivée, 1,5 s
    /// après (comme `ExtractSpec::default`), queue diffuse gaussienne de RT60
    /// `rt` dans toutes les bandes, bruit de fond stationnaire `bruit_db` sous
    /// le niveau efficace de la queue à son départ.
    fn ri_bruitee(rt: f64, bruit_db: f64, graine: u64) -> ImpulseResponse {
        let fs = 48_000u32;
        let pre = (0.15 * fs as f64) as usize;
        let n = pre + (1.5 * fs as f64) as usize;
        let mut etat = graine.max(1);
        let mut unif = move || {
            etat ^= etat << 13;
            etat ^= etat >> 7;
            etat ^= etat << 17;
            ((etat >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let mut gauss = move || {
            let (u1, u2) = (unif(), unif());
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        };
        let sigma_bruit = 10f64.powf(bruit_db / 20.0);
        let samples = (0..n)
            .map(|i| {
                let queue = if i >= pre {
                    let t = (i - pre) as f64 / fs as f64;
                    gauss() * 10f64.powf(-3.0 * t / rt)
                } else {
                    0.0
                };
                queue + sigma_bruit * gauss()
            })
            .collect();
        ImpulseResponse {
            sample_rate: fs,
            samples,
            peak_index: pre,
            delay_samples: pre,
        }
    }

    const BANDES: [f64; 6] = [125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0];

    /// Le défaut vu chez un testeur (25/11, RSB faible) : l'intégrale de
    /// Schroeder menée jusqu'au bout de la réponse, sans troncature, donne des
    /// RT60 de 5 à 9 s pour une pièce à 0,5 s dès que le bruit de fond est à
    /// −20 ou −30 dB (et un T30 de plus de 4 s encore à −40 dB).
    #[test]
    fn sans_troncature_le_bruit_gonfle_le_rt60() {
        let rt = 0.5;
        for (bruit, min_attendu) in [(-20.0, 2.5), (-30.0, 2.5)] {
            let ir = ri_bruitee(rt, bruit, 7);
            let start = ir.peak_index - 240;
            let h = &ir.samples[start..];
            for fc in [250.0, 500.0, 1000.0, 2000.0] {
                let band =
                    bandpass_zero_phase(h, ir.sample_rate, fc / 2f64.sqrt(), fc * 2f64.sqrt());
                let d = schroeder_decay_db(&band);
                let t20 = decay_time(&d, ir.sample_rate, -5.0, -25.0);
                assert!(
                    t20.is_finite() && t20 > min_attendu,
                    "bruit {bruit} dB, {fc} Hz : T20 naïf {t20:.2} s, on attendait le défaut (> {min_attendu} s)"
                );
            }
        }
    }

    /// Le correctif : tronqué au croisement avec le bruit, chaque temps rendu
    /// est juste (±20 %) ; sinon il est NaN (« non mesurable »), jamais faux.
    ///
    /// L'EDT n'est jugé qu'à partir de 250 Hz : sur 80 ms, une octave grave
    /// d'une queue aléatoire fluctue de ±30 % même sans aucun bruit.
    #[test]
    fn tronque_au_bruit_le_rt60_est_juste_ou_non_mesurable() {
        let rt = 0.5;
        for (bruit, graine) in [(-20.0, 3), (-30.0, 5), (-40.0, 11)] {
            let ir = ri_bruitee(rt, bruit, graine);
            let bandes = rt60_by_band(&ir, &BANDES, 1.0);
            for b in &bandes {
                assert!(b.dynamic_range_db.is_finite());
                for (nom, v) in [("EDT", b.edt_s), ("T20", b.t20_s), ("T30", b.t30_s)] {
                    if nom == "EDT" && b.center_hz < 250.0 {
                        continue;
                    }
                    assert!(
                        !v.is_finite() || (v - rt).abs() / rt < 0.2,
                        "bruit {bruit} dB, {} Hz : {nom} {v:.2} s pour {rt} s (dyn {:.0} dB)",
                        b.center_hz,
                        b.dynamic_range_db
                    );
                }
                // La plage dynamique suit le bruit de fond (à quelques dB près).
                assert!(
                    (b.dynamic_range_db + bruit).abs() < 6.0,
                    "bruit {bruit} dB, {} Hz : dyn {:.1} dB",
                    b.center_hz,
                    b.dynamic_range_db
                );
                // 45 dB manquent partout : pas de T30.
                assert!(b.t30_s.is_nan(), "{} Hz : T30 {:.2}", b.center_hz, b.t30_s);
            }
            // Ce qui reste mesurable l'est bien.
            let mesurees =
                |f: fn(&BandRt60) -> f64| bandes.iter().filter(|b| f(b).is_finite()).count();
            match bruit as i32 {
                -40 => {
                    // 35 dB pour T20 : à −40 dB, la plupart des bandes y sont.
                    assert_eq!(mesurees(|b| b.edt_s), BANDES.len());
                    assert!(mesurees(|b| b.t20_s) >= 4);
                }
                -30 => {
                    assert_eq!(mesurees(|b| b.edt_s), BANDES.len());
                    assert_eq!(mesurees(|b| b.t20_s), 0);
                }
                _ => assert_eq!(mesurees(|b| b.t20_s), 0),
            }
        }
    }

    /// Sans bruit, la troncature ne change rien : même T30 que l'intégrale
    /// complète (à 3 %), et à 10 % du RT60 simulé (fluctuation de la queue
    /// aléatoire, la même pour les deux méthodes).
    #[test]
    fn sans_bruit_la_troncature_ne_change_rien() {
        let rt = 0.5;
        let ir = ri_bruitee(rt, -100.0, 13);
        let start = ir.peak_index - 240;
        let h = &ir.samples[start..];
        for b in rt60_by_band(&ir, &BANDES, 1.0) {
            let fc = b.center_hz;
            let band = bandpass_zero_phase(h, ir.sample_rate, fc / 2f64.sqrt(), fc * 2f64.sqrt());
            let complet = decay_time(&schroeder_decay_db(&band), ir.sample_rate, -5.0, -35.0);
            assert!(
                (b.t30_s - complet).abs() / complet < 0.03 && (b.t30_s - rt).abs() / rt < 0.1,
                "{fc} Hz : T30 {:.3} s, intégrale complète {complet:.3} s (EDT {:.3}, T20 {:.3}, dyn {:.0} dB)",
                b.t30_s,
                b.edt_s,
                b.t20_s,
                b.dynamic_range_db
            );
        }
    }

    /// Du bruit seul : aucune décroissance, aucune valeur.
    #[test]
    fn du_bruit_seul_n_est_pas_mesurable() {
        let mut ir = ri_bruitee(0.5, 0.0, 17);
        let pre = ir.peak_index;
        // On remplace la queue par du bruit de même niveau que l'avant.
        let avant: Vec<f64> = ir.samples[..pre].to_vec();
        for (i, v) in ir.samples.iter_mut().enumerate().skip(pre) {
            *v = avant[i % pre];
        }
        for b in rt60_by_band(&ir, &BANDES, 1.0) {
            assert!(
                b.edt_s.is_nan() && b.t20_s.is_nan() && b.t30_s.is_nan(),
                "{b:?}"
            );
            assert!(b.dynamic_range_db.is_finite());
        }
    }
}
