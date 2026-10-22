//! Statistiques élémentaires : centiles, gigue, intervalle de Wilson.

/// Quantile `q` (0..=1) par interpolation linéaire entre rangs (méthode 7 de
/// Hyndman et Fan, celle de R et de numpy par défaut). `None` si vide.
pub fn quantile(valeurs: &[f64], q: f64) -> Option<f64> {
    if valeurs.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = valeurs.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let h = (v.len() - 1) as f64 * q.clamp(0.0, 1.0);
    let bas = h.floor() as usize;
    let haut = h.ceil() as usize;
    Some(v[bas] + (h - bas as f64) * (v[haut] - v[bas]))
}

pub fn mediane(valeurs: &[f64]) -> Option<f64> {
    quantile(valeurs, 0.5)
}

/// Gigue à la manière de la RFC 3550 : moyenne des écarts absolus entre RTT
/// successifs (dans l'ordre d'émission des pings).
pub fn gigue(rtts_dans_l_ordre: &[f64]) -> Option<f64> {
    if rtts_dans_l_ordre.len() < 2 {
        return None;
    }
    let somme: f64 = rtts_dans_l_ordre
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .sum();
    Some(somme / (rtts_dans_l_ordre.len() - 1) as f64)
}

/// z pour un intervalle bilatéral à 95 %.
pub const Z_95: f64 = 1.959_963_984_540_054;

/// Intervalle de score de Wilson à 95 % pour `succes` sur `n`. `None` si `n == 0`.
///
/// Choisi plutôt que l'intervalle de Wald parce que les cellules sont petites
/// (20 essais) et que le taux peut valoir 0 ou 1 : Wald y donne un intervalle
/// de largeur nulle, Wilson non.
pub fn wilson(succes: u64, n: u64) -> Option<(f64, f64)> {
    if n == 0 {
        return None;
    }
    let n = n as f64;
    let p = succes.min(n as u64) as f64 / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let demi = Z_95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    Some(((centre - demi).max(0.0), (centre + demi).min(1.0)))
}

pub fn arrondi(x: f64, decimales: i32) -> f64 {
    let f = 10f64.powi(decimales);
    (x * f).round() / f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proche(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn wilson_valeurs_de_reference() {
        // Valeurs de référence (Newcombe 1998, tableau I, méthode 3) :
        // 81/263 → [0,2553 ; 0,3662], 15/148 → [0,0624 ; 0,1605].
        let (b, h) = wilson(81, 263).unwrap();
        assert!(
            proche(b, 0.2553) && proche(h, 0.3662),
            "Wilson 81/263 attendu [0,2553 ; 0,3662], obtenu [{b} ; {h}]"
        );
        let (b, h) = wilson(15, 148).unwrap();
        assert!(proche(b, 0.0624) && proche(h, 0.1605), "15/148 → {b} {h}");
    }

    #[test]
    fn wilson_aux_bords_n_est_pas_degenere() {
        // 20/20 : Wald donnerait [1 ; 1], Wilson [0,8389 ; 1].
        let (b, h) = wilson(20, 20).unwrap();
        assert!(proche(b, 0.8389), "20/20 borne basse {b}");
        assert!(proche(h, 1.0));
        let (b, h) = wilson(0, 20).unwrap();
        assert!(proche(b, 0.0));
        assert!(proche(h, 0.1611), "0/20 borne haute {h}");
        // 10/20 : environ ±20 points autour de 50 % (cahier § 4.2).
        let (b, h) = wilson(10, 20).unwrap();
        assert!(proche(b, 0.2993) && proche(h, 0.7007), "10/20 → {b} {h}");
        assert_eq!(wilson(0, 0), None);
    }

    #[test]
    fn quantiles_par_interpolation() {
        let v = [5.0, 1.0, 4.0, 2.0, 3.0];
        assert_eq!(mediane(&v), Some(3.0));
        assert_eq!(quantile(&v, 0.95), Some(4.8));
        assert_eq!(quantile(&[7.0], 0.99), Some(7.0));
        assert_eq!(mediane(&[1.0, 2.0]), Some(1.5));
        assert_eq!(mediane(&[]), None);
    }

    #[test]
    fn gigue_moyenne_des_ecarts_successifs() {
        assert_eq!(
            gigue(&[10.0, 12.0, 9.0, 9.0]),
            Some((2.0 + 3.0 + 0.0) / 3.0)
        );
        assert_eq!(gigue(&[10.0]), None);
    }
}
