//! Autoregressive model: windowed autocorrelation, Levinson-Durbin, and the
//! LSAR interpolator (Godsill & Rayner). Everything works in caller-owned
//! buffers: no allocation, suitable for the audio thread.

/// Autocorrelation `r[0..=p]` of `x` (already windowed by the caller).
pub fn autocorrelation(x: &[f64], r: &mut [f64]) {
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut acc = 0.0;
        if lag < x.len() {
            for (a, b) in x[lag..].iter().zip(x.iter()) {
                acc += a * b;
            }
        }
        *slot = acc;
    }
}

/// Levinson-Durbin. Writes `a[0..=p]` with `a[0] = 1` so that the prediction
/// error is `e[n] = Σ a_k x[n-k]`. `tmp` holds at least `p + 1` values.
/// Returns the final prediction error power, or `None` when `r[0]` is not
/// positive or the recursion becomes unstable (the caller then skips).
pub fn levinson(r: &[f64], a: &mut [f64], tmp: &mut [f64]) -> Option<f64> {
    let p = r.len() - 1;
    if r[0].is_nan() || r[0] <= 0.0 || !r[0].is_finite() {
        return None;
    }
    a.fill(0.0);
    a[0] = 1.0;
    let mut err = r[0];
    for i in 1..=p {
        let mut acc = r[i];
        for j in 1..i {
            acc += a[j] * r[i - j];
        }
        let k = -acc / err;
        if !k.is_finite() || k.abs() >= 1.0 {
            return None;
        }
        tmp[..i].copy_from_slice(&a[..i]);
        for j in 1..i {
            a[j] = tmp[j] + k * tmp[i - j];
        }
        a[i] = k;
        err *= 1.0 - k * k;
        if err.is_nan() || err <= 0.0 {
            return None;
        }
    }
    Some(err)
}

/// `R(d) = Σ_k a_k a_{k+d}` for `d = 0..=p`: the band of `A_uᵀ A_u`.
pub fn coefficient_autocorrelation(a: &[f64], out: &mut [f64]) {
    let p = a.len() - 1;
    for (d, slot) in out.iter_mut().enumerate().take(p + 1) {
        *slot = a[..=p - d].iter().zip(&a[d..]).map(|(x, y)| x * y).sum();
    }
}

/// Band Cholesky factor of the `m × m` LSAR matrix `R(|i-j|)` (`band` =
/// `R(0..=p)`), in `chol[i*(p+1) + d] = L[i][i-d]`. False if not positive
/// definite.
pub fn factor(band: &[f64], m: usize, chol: &mut [f64]) -> bool {
    let p = band.len() - 1;
    let bw = p + 1;
    for i in 0..m {
        let j0 = i.saturating_sub(p);
        for j in j0..=i {
            let mut sum = band[i - j];
            let k0 = j0.max(j.saturating_sub(p));
            for k in k0..j {
                sum -= chol[i * bw + (i - k)] * chol[j * bw + (j - k)];
            }
            if i == j {
                if sum.is_nan() || sum <= 0.0 {
                    return false;
                }
                chol[i * bw] = sum.sqrt();
            } else {
                chol[i * bw + (i - j)] = sum / chol[j * bw];
            }
        }
    }
    true
}

/// LSAR estimate of `w[start..end]` from the `p` known samples on each side,
/// with the factor of [`factor`] for `m = end - start`. Leaves `w` untouched
/// and writes the estimate to `out[..m]`. `ek` holds at least `m + p`
/// values. False if the result is not finite.
pub fn solve(
    w: &[f64],
    start: usize,
    end: usize,
    a: &[f64],
    chol: &[f64],
    ek: &mut [f64],
    out: &mut [f64],
) -> bool {
    let p = a.len() - 1;
    let m = end - start;
    let bw = p + 1;
    debug_assert!(start >= p && end + p <= w.len() && m > 0);
    // ek[n - start] = prediction error with the unknown samples set to zero.
    for (i, slot) in ek[..m + p].iter_mut().enumerate() {
        let n = start + i;
        let mut acc = 0.0;
        for (k, ak) in a.iter().enumerate() {
            let idx = n - k;
            if idx < start || idx >= end {
                acc += ak * w[idx];
            }
        }
        *slot = acc;
    }
    // Normal equations R x = -Aᵀ ek, then L y = rhs, Lᵀ x = y.
    for (i, slot) in out[..m].iter_mut().enumerate() {
        *slot = -a
            .iter()
            .enumerate()
            .map(|(k, ak)| ak * ek[i + k])
            .sum::<f64>();
    }
    for i in 0..m {
        let mut sum = out[i];
        for k in i.saturating_sub(p)..i {
            sum -= chol[i * bw + (i - k)] * out[k];
        }
        out[i] = sum / chol[i * bw];
    }
    for i in (0..m).rev() {
        let mut sum = out[i];
        for k in i + 1..(i + p + 1).min(m) {
            sum -= chol[k * bw + (k - i)] * out[k];
        }
        out[i] = sum / chol[i * bw];
    }
    out[..m].iter().all(|v| v.is_finite())
}

/// LSAR interpolation of `w[start..end]` in place ([`factor`] then
/// [`solve`]). Returns false and leaves `w` untouched on failure.
#[allow(clippy::too_many_arguments)]
pub fn interpolate(
    w: &mut [f64],
    start: usize,
    end: usize,
    a: &[f64],
    band: &[f64],
    chol: &mut [f64],
    ek: &mut [f64],
    rhs: &mut [f64],
) -> bool {
    let m = end - start;
    if !factor(band, m, chol) || !solve(w, start, end, a, chol, ek, rhs) {
        return false;
    }
    w[start..end].copy_from_slice(&rhs[..m]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levinson_recovers_an_ar2_process() {
        // x[n] = 1.6 x[n-1] - 0.8 x[n-2] + white
        let mut x = vec![0.0; 50_000];
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for n in 2..x.len() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let u = (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            x[n] = 1.6 * x[n - 1] - 0.8 * x[n - 2] + u;
        }
        let mut r = [0.0; 3];
        autocorrelation(&x, &mut r);
        let (mut a, mut t) = ([0.0; 3], [0.0; 3]);
        levinson(&r, &mut a, &mut t).unwrap();
        assert!(
            (a[1] + 1.6).abs() < 0.02 && (a[2] - 0.8).abs() < 0.02,
            "{a:?}"
        );
    }

    #[test]
    fn lsar_restores_a_sine_gap_exactly() {
        // A pure sinusoid obeys x[n] = 2cos(w) x[n-1] - x[n-2] exactly.
        let wv = 0.05f64;
        let clean: Vec<f64> = (0..400).map(|n| (wv * n as f64).sin()).collect();
        let a = [1.0, -2.0 * wv.cos(), 1.0];
        let mut band = [0.0; 3];
        coefficient_autocorrelation(&a, &mut band);
        let mut w = clean.clone();
        for v in &mut w[200..230] {
            *v = 0.9;
        }
        let (mut chol, mut ek, mut rhs) = (vec![0.0; 30 * 3], vec![0.0; 32], vec![0.0; 30]);
        assert!(interpolate(
            &mut w, 200, 230, &a, &band, &mut chol, &mut ek, &mut rhs
        ));
        for (x, y) in w.iter().zip(&clean) {
            assert!((x - y).abs() < 1e-9, "{x} vs {y}");
        }
    }
}
