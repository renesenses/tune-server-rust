//! Fenêtres de pondération d'un sinc tronqué, et les formules de Kaiser qui
//! relient atténuation, largeur de transition et longueur.
//!
//! Références publiques :
//! * J. F. Kaiser, « Nonrecursive digital filter design using the I0-sinh
//!   window function », Proc. IEEE ISCAS, 1974 (formules empiriques β(A) et
//!   N(A, Δf)).
//! * F. J. Harris, « On the use of windows for harmonic analysis with the
//!   discrete Fourier transform », Proc. IEEE 66(1), 1978 (Blackman-Harris,
//!   gaussienne, comparaison des lobes).
//! * A. V. Oppenheim, R. W. Schafer, *Discrete-Time Signal Processing*,
//!   chap. 7 (conception par fenêtrage).

use std::f64::consts::PI;

/// Fenêtre gaussienne de `n` points. `alpha` est le rapport entre la
/// demi-longueur et l'écart-type : `w = exp(−½ (alpha · x)²)` avec `x ∈ [−1, 1]`.
/// Les bords valent `exp(−alpha²/2)` : −109 dB pour `alpha = 5`, −154 dB
/// pour `alpha = 6`. Son spectre est lui-même gaussien, donc **sans lobes
/// secondaires** : la réponse hors bande décroît de façon monotone — c'est la
/// propriété recherchée pour un filtre apodisant.
pub fn gaussienne(n: usize, alpha: f64) -> Vec<f64> {
    assert!(n >= 2 && alpha > 0.0);
    let c = (n - 1) as f64 / 2.0;
    (0..n)
        .map(|i| {
            let x = (i as f64 - c) / c;
            (-0.5 * (alpha * x).powi(2)).exp()
        })
        .collect()
}

/// Fenêtre de Kaiser de `n` points et de paramètre `beta`.
pub fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    assert!(n >= 2 && beta >= 0.0);
    let c = (n - 1) as f64 / 2.0;
    let denom = bessel_i0(beta);
    (0..n)
        .map(|i| {
            let x = (i as f64 - c) / c;
            bessel_i0(beta * (1.0 - x * x).max(0.0).sqrt()) / denom
        })
        .collect()
}

/// Blackman-Harris à quatre termes (lobes secondaires à −92 dB).
pub fn blackman_harris(n: usize) -> Vec<f64> {
    assert!(n >= 2);
    let a = [0.35875, 0.48829, 0.14128, 0.01168];
    (0..n)
        .map(|i| {
            let t = 2.0 * PI * i as f64 / (n - 1) as f64;
            a[0] - a[1] * t.cos() + a[2] * (2.0 * t).cos() - a[3] * (3.0 * t).cos()
        })
        .collect()
}

/// Fonction de Bessel modifiée de première espèce, ordre 0, par sa série.
pub fn bessel_i0(x: f64) -> f64 {
    let y = x * x / 4.0;
    let mut terme = 1.0;
    let mut somme = 1.0;
    let mut k = 1.0;
    loop {
        terme *= y / (k * k);
        somme += terme;
        if terme < somme * 1e-17 {
            return somme;
        }
        k += 1.0;
    }
}

/// `β` de Kaiser pour une atténuation `a_db` (formule empirique de Kaiser).
pub fn kaiser_beta(a_db: f64) -> f64 {
    if a_db > 50.0 {
        0.1102 * (a_db - 8.7)
    } else if a_db >= 21.0 {
        0.5842 * (a_db - 21.0).powf(0.4) + 0.07886 * (a_db - 21.0)
    } else {
        0.0
    }
}

/// Largeur de transition (en fraction de la cadence) qu'un sinc de `n`
/// coefficients fenêtré par Kaiser rend pour une atténuation `a_db` :
/// `Δf = (A − 8) / (2,285 · 2π · (n − 1))`.
pub fn kaiser_transition(n: usize, a_db: f64) -> f64 {
    (a_db - 8.0) / (2.285 * 2.0 * PI * (n - 1) as f64)
}

/// Longueur nécessaire pour tenir `a_db` sur une transition `delta_f`
/// (fraction de la cadence), arrondie au multiple de deux supérieur.
pub fn kaiser_longueur(a_db: f64, delta_f: f64) -> usize {
    let n = (a_db - 8.0) / (2.285 * 2.0 * PI * delta_f) + 1.0;
    let n = n.ceil() as usize;
    n + (n % 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bessel_valeurs_connues() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008).abs() < 1e-12);
        assert!((bessel_i0(5.0) - 27.239_871_823_604_44).abs() < 1e-9);
    }

    #[test]
    fn fenetres_symetriques_et_bornees() {
        for w in [gaussienne(65, 5.0), kaiser(65, 12.0), blackman_harris(65)] {
            assert!((w[32] - 1.0).abs() < 1e-9 || w[32] <= 1.0 + 1e-9);
            for i in 0..65 {
                assert!((w[i] - w[64 - i]).abs() < 1e-12);
                assert!(w[i] >= -1e-12 && w[i] <= 1.0 + 1e-12);
            }
        }
    }

    #[test]
    fn kaiser_formules_coherentes() {
        let n = kaiser_longueur(150.0, 0.01);
        let d = kaiser_transition(n, 150.0);
        assert!(d <= 0.01 && d > 0.009);
        assert!((kaiser_beta(150.0) - 15.571).abs() < 1e-3);
    }
}
