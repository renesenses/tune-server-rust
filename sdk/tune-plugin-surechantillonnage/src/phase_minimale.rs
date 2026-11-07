//! Phase minimale par la méthode du cepstre (homomorphique).
//!
//! Référence publique : Oppenheim & Schafer, *Discrete-Time Signal
//! Processing*, § 13.6 (« minimum-phase/all-pass decomposition » par le
//! cepstre réel) ; J. O. Smith, *Introduction to Digital Filters*, section
//! « Creating Minimum Phase Filters and Signals ».
//!
//! Le module est conservé ; toute l'énergie est ramenée au début de la
//! réponse. Le prix : la phase n'est plus linéaire (délai de groupe variable
//! dans la bande passante), et le plancher numérique du procédé — le
//! logarithme d'un module qui tend vers zéro — borne la bande atténuée à
//! environ `PLANCHER_DB`. Le banc mesure ce que cela donne.

use crate::fft::{fft, puissance_de_deux};

/// Plancher imposé au module avant le logarithme, en dB sous le maximum.
pub const PLANCHER_DB: f64 = -190.0;

/// Transforme `h` en sa version à phase minimale, de même longueur.
pub fn vers_phase_minimale(h: &[f64]) -> Vec<f64> {
    let n = h.len();
    // Bourrage généreux : le cepstre d'un filtre à bande atténuée profonde
    // décroît lentement ; 16× la longueur évite le repliement du cepstre.
    let nfft = puissance_de_deux(n * 16).max(1 << 12);
    let mut re = vec![0.0; nfft];
    let mut im = vec![0.0; nfft];
    re[..n].copy_from_slice(h);
    fft(&mut re, &mut im, false);
    let max = re
        .iter()
        .zip(&im)
        .map(|(a, b)| a.hypot(*b))
        .fold(0.0_f64, f64::max);
    let plancher = max * 10f64.powf(PLANCHER_DB / 20.0);
    for k in 0..nfft {
        let m = re[k].hypot(im[k]).max(plancher);
        re[k] = m.ln();
        im[k] = 0.0;
    }
    // Cepstre réel.
    fft(&mut re, &mut im, true);
    // Pliage : partie causale doublée, anticausale annulée.
    let demi = nfft / 2;
    for v in &mut re[1..demi] {
        *v *= 2.0;
    }
    re[demi + 1..].fill(0.0);
    for v in im.iter_mut() {
        *v = 0.0;
    }
    fft(&mut re, &mut im, false);
    // exp(a + ib)
    for k in 0..nfft {
        let g = re[k].exp();
        let (s, c) = im[k].sin_cos();
        re[k] = g * c;
        im[k] = g * s;
    }
    fft(&mut re, &mut im, true);
    re.truncate(n);
    re
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conception::{Fenetre, sinc_fenetre};
    use crate::fft::module_spectre;

    #[test]
    fn module_conserve_et_energie_devant() {
        let h = sinc_fenetre(512, 0.11, Fenetre::Kaiser { beta: 10.0 });
        let m = vers_phase_minimale(&h);
        assert_eq!(m.len(), h.len());
        let nfft = 8192;
        let a = module_spectre(&h, nfft);
        let b = module_spectre(&m, nfft);
        // Module conservé dans la bande passante. La réponse à phase minimale
        // est tronquée à la longueur d'origine : l'écart croît vers le bord de
        // la bande. Mesuré le 04/11 : ~1e-5 dB vers le bin 50, 2,0e-4 dB au
        // bin 219, 2,16e-3 dB au bin 736 (bord). L'ancien seuil unique de
        // 1e-4 dB jusqu'au bin 737 ne tenait pas ; deux paliers.
        for k in 0..(nfft as f64 * 0.09) as usize {
            let d = 20.0 * (b[k] / a[k]).log10();
            let seuil = if k < nfft / 100 { 1e-4 } else { 5e-3 };
            assert!(d.abs() < seuil, "bin {k} : {d} dB (seuil {seuil} dB)");
        }
        // Barycentre de l'énergie nettement plus tôt.
        let centre = |v: &[f64]| {
            let e: f64 = v.iter().map(|x| x * x).sum();
            v.iter()
                .enumerate()
                .map(|(i, x)| i as f64 * x * x)
                .sum::<f64>()
                / e
        };
        assert!(
            centre(&m) < 0.3 * centre(&h),
            "{} vs {}",
            centre(&m),
            centre(&h)
        );
    }
}
