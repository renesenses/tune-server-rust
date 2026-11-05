//! Outils FFT partagés : convolution linéaire, spectre d'une réponse
//! impulsionnelle, intercorrélation. Tout est en f64 : l'analyse n'a pas
//! de contrainte temps réel, la précision prime sur le coût.

use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

/// Puissance de deux supérieure ou égale à `n` (au moins 2).
pub fn next_pow2(n: usize) -> usize {
    let mut p = 2;
    while p < n {
        p <<= 1;
    }
    p
}

/// Transformée réelle directe, remplissage à zéro jusqu'à `nfft`.
/// Renvoie `nfft/2 + 1` bins.
pub fn rfft(x: &[f64], nfft: usize) -> Vec<Complex<f64>> {
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(nfft);
    let mut input = vec![0.0; nfft];
    let n = x.len().min(nfft);
    input[..n].copy_from_slice(&x[..n]);
    let mut output = r2c.make_output_vec();
    r2c.process(&mut input, &mut output)
        .expect("rfft : tailles cohérentes par construction");
    output
}

/// Transformée réelle inverse, normalisée (x = irfft(rfft(x))).
pub fn irfft(spectrum: &[Complex<f64>], nfft: usize) -> Vec<f64> {
    let mut planner = RealFftPlanner::<f64>::new();
    let c2r = planner.plan_fft_inverse(nfft);
    let mut input = spectrum.to_vec();
    assert_eq!(
        input.len(),
        nfft / 2 + 1,
        "irfft : nombre de bins incohérent"
    );
    // Les parties imaginaires de DC et de Nyquist n'ont pas de sens pour un
    // signal réel ; on les force à zéro avant la transformée.
    input[0].im = 0.0;
    if nfft % 2 == 0 {
        let last = input.len() - 1;
        input[last].im = 0.0;
    }
    let mut output = c2r.make_output_vec();
    c2r.process(&mut input, &mut output)
        .expect("irfft : tailles cohérentes par construction");
    let scale = 1.0 / nfft as f64;
    output.iter_mut().for_each(|v| *v *= scale);
    output
}

/// Convolution linéaire complète (longueur `a.len() + b.len() - 1`) par FFT.
pub fn convolve(a: &[f64], b: &[f64]) -> Vec<f64> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let out_len = a.len() + b.len() - 1;
    let nfft = next_pow2(out_len);
    let fa = rfft(a, nfft);
    let fb = rfft(b, nfft);
    let prod: Vec<Complex<f64>> = fa.iter().zip(&fb).map(|(x, y)| x * y).collect();
    let mut y = irfft(&prod, nfft);
    y.truncate(out_len);
    y
}

/// Intercorrélation `r[k] = Σ x[n+k]·y[n]` pour `k ∈ [0, x.len())`, par FFT.
/// Sert à retrouver le retard d'un repère connu `y` dans une capture `x`.
pub fn cross_correlate(x: &[f64], y: &[f64]) -> Vec<f64> {
    if x.is_empty() || y.is_empty() {
        return Vec::new();
    }
    let nfft = next_pow2(x.len() + y.len());
    let fx = rfft(x, nfft);
    let fy = rfft(y, nfft);
    let prod: Vec<Complex<f64>> = fx.iter().zip(&fy).map(|(a, b)| a * b.conj()).collect();
    let mut r = irfft(&prod, nfft);
    r.truncate(x.len());
    r
}

/// Indice du maximum absolu d'un signal.
pub fn argmax_abs(x: &[f64]) -> usize {
    let mut best = 0;
    let mut best_v = f64::NEG_INFINITY;
    for (i, v) in x.iter().enumerate() {
        if v.abs() > best_v {
            best_v = v.abs();
            best = i;
        }
    }
    best
}

/// Conversion amplitude → dB (plancher à -200 dB).
pub fn to_db(amp: f64) -> f64 {
    20.0 * amp.abs().max(1e-10).log10()
}

/// Conversion dB → amplitude linéaire.
pub fn from_db(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_lineaire_egale_la_definition() {
        let a = [1.0, 2.0, 3.0];
        let b = [0.5, -1.0];
        let y = convolve(&a, &b);
        let attendu = [0.5, 0.0, -0.5, -3.0];
        assert_eq!(y.len(), 4);
        for (v, e) in y.iter().zip(attendu) {
            assert!((v - e).abs() < 1e-9, "{v} ≠ {e}");
        }
    }

    #[test]
    fn irfft_inverse_rfft() {
        let x: Vec<f64> = (0..64).map(|i| ((i * 7) % 13) as f64 - 6.0).collect();
        let y = irfft(&rfft(&x, 64), 64);
        for (a, b) in x.iter().zip(&y) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn intercorrelation_retrouve_un_retard() {
        let marker: Vec<f64> = (0..200)
            .map(|i| ((i * 37 % 11) as f64 - 5.0) / 5.0)
            .collect();
        let delay = 1234;
        let mut x = vec![0.0; 5000];
        x[delay..delay + marker.len()].copy_from_slice(&marker);
        let r = cross_correlate(&x, &marker);
        assert_eq!(argmax_abs(&r), delay);
    }
}
