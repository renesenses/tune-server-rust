//! Esquisse C3 : filtre FIR de correction en phase mixte et coût de la
//! convolution dans `tune-core/src/audio/convolver.rs`.
//!
//! - `minimum_phase_from_magnitude` : réponse impulsionnelle à phase minimale
//!   d'une magnitude donnée, par le cepstre réel (Oppenheim & Schafer,
//!   « Discrete-Time Signal Processing », § 13.5 ; même méthode que
//!   `scipy.signal.minimum_phase(method="homomorphic")`).
//! - `mixed_phase_design` : en dessous de `split_hz`, correction en phase
//!   minimale de la magnitude de l'égaliseur (les modes de pièce sont
//!   eux-mêmes à phase minimale, d'où l'absence de pré-écho) ; au-dessus,
//!   identité à phase linéaire (le FIR n'intervient pas, la correction HF
//!   restant à l'égaliseur IIR). Le résultat est un FIR prêt pour
//!   `ConvolverConfig::new` (f32, un vecteur par canal).
//! - `convolution_cost` : ordre de grandeur du coût CPU du convolueur
//!   partitionné uniforme de Tune (bloc 1024, FFT 2048) par canal.

use crate::fft;
use realfft::num_complex::Complex;

/// Réponse impulsionnelle à phase minimale de longueur `nfft` dont le module
/// vaut `magnitude_db(f)` (fonction continue de la fréquence en Hz).
pub fn minimum_phase_from_magnitude(
    sample_rate: u32,
    nfft: usize,
    magnitude_db: impl Fn(f64) -> f64,
) -> Vec<f64> {
    let nfft = fft::next_pow2(nfft);
    let bins = nfft / 2 + 1;
    let df = sample_rate as f64 / nfft as f64;
    // log|H| sur le spectre complet (symétrie hermitienne implicite via rfft/irfft).
    let log_mag: Vec<Complex<f64>> = (0..bins)
        .map(|i| {
            Complex::new(
                fft::from_db(magnitude_db(i as f64 * df)).max(1e-9).ln(),
                0.0,
            )
        })
        .collect();
    let cep = fft::irfft(&log_mag, nfft); // cepstre réel
                                          // Repliement : c[0], 2c[n] pour 0<n<N/2, c[N/2], 0 ensuite.
    let mut folded = vec![0.0; nfft];
    folded[0] = cep[0];
    for n in 1..nfft / 2 {
        folded[n] = 2.0 * cep[n];
    }
    folded[nfft / 2] = cep[nfft / 2];
    let spec = fft::rfft(&folded, nfft);
    let h_spec: Vec<Complex<f64>> = spec.iter().map(|c| c.exp()).collect();
    fft::irfft(&h_spec, nfft)
}

/// FIR en phase mixte de `taps` coefficients (voir en-tête du module).
/// `correction_db(f)` est la réponse souhaitée (par exemple celle de
/// l'égaliseur automatique, `eq::bands_response_db`). Au-dessus de
/// `split_hz` la correction est fondue vers 0 dB sur une demi-octave.
pub fn mixed_phase_design(
    sample_rate: u32,
    taps: usize,
    split_hz: f64,
    correction_db: impl Fn(f64) -> f64,
) -> Vec<f32> {
    let nfft = fft::next_pow2(taps * 4);
    let hi = split_hz * 2f64.sqrt();
    let mag = |f: f64| -> f64 {
        if f <= split_hz {
            correction_db(f)
        } else if f < hi {
            let t = (f / split_hz).ln() / (hi / split_hz).ln();
            correction_db(f) * (0.5 + 0.5 * (std::f64::consts::PI * t).cos())
        } else {
            0.0
        }
    };
    let mut h = minimum_phase_from_magnitude(sample_rate, nfft, mag);
    h.truncate(taps);
    // Fondu de sortie sur le dernier quart pour éviter une troncature brutale.
    let fade = taps / 4;
    for i in 0..fade {
        let w = 0.5 + 0.5 * (std::f64::consts::PI * i as f64 / fade as f64).cos();
        h[taps - fade + i] *= w;
    }
    h.iter().map(|v| *v as f32).collect()
}

/// Module (dB) d'un FIR à la fréquence `f` (somme directe, comme
/// `convolver::fir_frequency_response`).
pub fn fir_magnitude_db(h: &[f32], sample_rate: u32, f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / sample_rate as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, v) in h.iter().enumerate() {
        re += *v as f64 * (w * n as f64).cos();
        im -= *v as f64 * (w * n as f64).sin();
    }
    fft::to_db((re * re + im * im).sqrt())
}

/// Coût estimé de la convolution partitionnée uniforme de Tune.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvolutionCost {
    pub sample_rate: u32,
    pub taps: usize,
    pub block: usize,
    pub partitions: usize,
    /// Latence de bloc (ms) = `block / fs` (`Convolver::latency_frames`).
    pub block_latency_ms: f64,
    /// Multiplications-accumulations complexes par seconde et par canal
    /// (produit spectral des partitions), hors FFT.
    pub cmacs_per_s: f64,
    /// Opérations flottantes approximatives par seconde et par canal, FFT
    /// comprises (5·N·log2 N par FFT, 2 FFT par bloc).
    pub flops_per_s: f64,
    /// Mémoire de la ligne à retard fréquentielle par canal (octets, f32 complexes).
    pub fdl_bytes: usize,
}

pub fn convolution_cost(sample_rate: u32, taps: usize, block: usize) -> ConvolutionCost {
    let fft_size = (block * 2).next_power_of_two();
    let bins = fft_size / 2 + 1;
    let partitions = taps.div_ceil(block);
    let blocks_per_s = sample_rate as f64 / block as f64;
    let cmacs = partitions as f64 * bins as f64 * blocks_per_s;
    let fft_flops = 2.0 * 5.0 * fft_size as f64 * (fft_size as f64).log2() * blocks_per_s;
    ConvolutionCost {
        sample_rate,
        taps,
        block,
        partitions,
        block_latency_ms: block as f64 * 1000.0 / sample_rate as f64,
        cmacs_per_s: cmacs,
        flops_per_s: cmacs * 8.0 + fft_flops,
        fdl_bytes: partitions * bins * 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq::{bands_response_db, Band, BandType};

    #[test]
    fn phase_minimale_restitue_la_magnitude() {
        let fs = 48_000;
        let bands = [Band {
            freq: 50.0,
            gain: -8.0,
            q: 4.0,
            kind: BandType::Peak,
            channel: None,
        }];
        let corr = |f: f64| bands_response_db(fs as f64, &bands, &[f])[0];
        let h = minimum_phase_from_magnitude(fs, 65_536, corr);
        let hf: Vec<f32> = h.iter().map(|v| *v as f32).collect();
        for f in [30.0, 50.0, 80.0, 200.0, 2000.0] {
            let got = fir_magnitude_db(&hf, fs, f);
            assert!((got - corr(f)).abs() < 0.3, "{f} Hz : {got} ≠ {}", corr(f));
        }
        // Phase minimale : l'énergie est concentrée au début.
        let e_total: f64 = h.iter().map(|v| v * v).sum();
        let e_head: f64 = h[..4096].iter().map(|v| v * v).sum();
        assert!(e_head / e_total > 0.99);
    }

    #[test]
    fn fir_mixte_colle_a_l_egaliseur_sous_la_coupure() {
        let fs = 48_000;
        let bands = [
            Band {
                freq: 42.0,
                gain: -9.0,
                q: 6.0,
                kind: BandType::Peak,
                channel: None,
            },
            Band {
                freq: 115.0,
                gain: -5.0,
                q: 7.0,
                kind: BandType::Peak,
                channel: None,
            },
        ];
        let corr = |f: f64| bands_response_db(fs as f64, &bands, &[f])[0];
        let h = mixed_phase_design(fs, 16_384, 300.0, corr);
        for f in [42.0, 60.0, 115.0, 200.0] {
            let got = fir_magnitude_db(&h, fs, f);
            assert!((got - corr(f)).abs() < 0.5, "{f} Hz : {got} ≠ {}", corr(f));
        }
        assert!(fir_magnitude_db(&h, fs, 2000.0).abs() < 0.2);
    }

    #[test]
    fn cout_convolution() {
        let c = convolution_cost(48_000, 16_384, 1024);
        assert_eq!(c.partitions, 16);
        assert!((c.block_latency_ms - 21.333).abs() < 0.01);
        assert!(c.flops_per_s > 1e7 && c.flops_per_s < 1e9);
    }
}
