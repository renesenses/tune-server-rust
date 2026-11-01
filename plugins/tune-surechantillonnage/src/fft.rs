//! FFT complexe radix-2 en f64, en place, sans dépendance.
//!
//! Elle sert à la conception (phase minimale par cepstre) et au banc de mesure
//! (réponses, spectres, bruit). Les facteurs de rotation sont calculés
//! directement par `cos`/`sin` pour chaque indice — pas par récurrence — afin
//! que l'erreur reste à la précision machine même à 2^22 points, ce qu'exige
//! la mesure de planchers à −150 dB.

use std::f64::consts::PI;

/// Transformée en place. `inverse = true` applique le facteur `1/n`.
pub fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    assert_eq!(n, im.len(), "parties réelle et imaginaire de tailles différentes");
    assert!(n.is_power_of_two(), "taille FFT non puissance de deux : {n}");
    if n < 2 {
        return;
    }
    // Permutation par renversement des bits.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let signe = if inverse { 1.0 } else { -1.0 };
    let demi = n / 2;
    let mut twr = Vec::with_capacity(demi);
    let mut twi = Vec::with_capacity(demi);
    for k in 0..demi {
        let a = signe * 2.0 * PI * k as f64 / n as f64;
        twr.push(a.cos());
        twi.push(a.sin());
    }
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let pas = n / len;
        let mut debut = 0;
        while debut < n {
            for k in 0..half {
                let wr = twr[k * pas];
                let wi = twi[k * pas];
                let i = debut + k;
                let j = i + half;
                let xr = re[j] * wr - im[j] * wi;
                let xi = re[j] * wi + im[j] * wr;
                re[j] = re[i] - xr;
                im[j] = im[i] - xi;
                re[i] += xr;
                im[i] += xi;
            }
            debut += len;
        }
        len <<= 1;
    }
    if inverse {
        let inv = 1.0 / n as f64;
        for v in re.iter_mut() {
            *v *= inv;
        }
        for v in im.iter_mut() {
            *v *= inv;
        }
    }
}

/// Plus petite puissance de deux ≥ `n`.
pub fn puissance_de_deux(n: usize) -> usize {
    n.next_power_of_two()
}

/// Spectre d'amplitude (module) d'un signal réel, bourré de zéros jusqu'à
/// `nfft`. Rend `nfft / 2 + 1` modules.
pub fn module_spectre(signal: &[f64], nfft: usize) -> Vec<f64> {
    assert!(signal.len() <= nfft, "signal plus long que la FFT");
    let mut re = vec![0.0; nfft];
    let mut im = vec![0.0; nfft];
    re[..signal.len()].copy_from_slice(signal);
    fft(&mut re, &mut im, false);
    (0..=nfft / 2).map(|k| re[k].hypot(im[k])).collect()
}

/// Densité spectrale de puissance moyennée (Welch) d'un signal réel, fenêtre
/// de Hann, recouvrement de moitié. Rend `taille / 2 + 1` puissances,
/// normalisées en PUISSANCE DE BRUIT : la somme de tous les bins d'un bruit
/// blanc de variance `σ²` vaut `σ²` (une raie pure rend `−1,76 dB` de moins
/// que sa puissance vraie, à cause de la fenêtre ; les rapports entre bandes
/// ne dépendent pas de cette normalisation).
pub fn dsp_welch(signal: &[f64], taille: usize) -> Vec<f64> {
    assert!(taille.is_power_of_two() && taille >= 16);
    let fen: Vec<f64> = (0..taille)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / taille as f64).cos())
        .collect();
    let somme_fen2: f64 = fen.iter().map(|w| w * w).sum();
    let norme = taille as f64 * somme_fen2;
    let mut acc = vec![0.0; taille / 2 + 1];
    let mut segments = 0usize;
    let mut re = vec![0.0; taille];
    let mut im = vec![0.0; taille];
    let mut debut = 0;
    while debut + taille <= signal.len() {
        for i in 0..taille {
            re[i] = signal[debut + i] * fen[i];
            im[i] = 0.0;
        }
        fft(&mut re, &mut im, false);
        for k in 0..=taille / 2 {
            let p = (re[k] * re[k] + im[k] * im[k]) / norme;
            // Spectre unilatéral : les bins intérieurs comptent deux fois.
            acc[k] += if k == 0 || k == taille / 2 { p } else { 2.0 * p };
        }
        segments += 1;
        debut += taille / 2;
    }
    assert!(segments > 0, "signal trop court pour un segment Welch");
    for v in acc.iter_mut() {
        *v /= segments as f64;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour_identite() {
        let n = 1024;
        let mut re: Vec<f64> = (0..n).map(|i| ((i * 7) % 13) as f64 - 6.0).collect();
        let mut im = vec![0.0; n];
        let orig = re.clone();
        fft(&mut re, &mut im, false);
        fft(&mut re, &mut im, true);
        for i in 0..n {
            assert!((re[i] - orig[i]).abs() < 1e-11, "bin {i}");
            assert!(im[i].abs() < 1e-11);
        }
    }

    #[test]
    fn raie_pure_au_bon_bin() {
        let n = 4096;
        let k0 = 37;
        let sig: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * k0 as f64 * i as f64 / n as f64).sin())
            .collect();
        let m = module_spectre(&sig, n);
        let (kmax, _) = m
            .iter()
            .enumerate()
            .fold((0, 0.0), |a, (k, &v)| if v > a.1 { (k, v) } else { a });
        assert_eq!(kmax, k0);
        assert!((m[k0] - n as f64 / 2.0).abs() < 1e-8);
    }
}
