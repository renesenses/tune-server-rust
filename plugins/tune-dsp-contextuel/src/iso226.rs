//! Courbes d'isosonie ISO 226:2003 (tableau 1 et formule (1) de la norme).
//!
//! `L_p(f, L_N)` : niveau de pression acoustique, en dB SPL, d'un son pur de
//! fréquence `f` perçu au niveau d'isosonie `L_N` phones. Défini de 20 Hz à
//! 12,5 kHz, sur les 29 fréquences du tableau.

/// Fréquences du tableau, en Hz.
pub const FREQUENCES: [f64; 29] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1_000.0, 1_250.0, 1_600.0, 2_000.0, 2_500.0, 3_150.0, 4_000.0, 5_000.0,
    6_300.0, 8_000.0, 10_000.0, 12_500.0,
];

/// Exposant de la perception de l'intensité, `α_f`.
const ALPHA_F: [f64; 29] = [
    0.532, 0.506, 0.480, 0.455, 0.432, 0.409, 0.387, 0.367, 0.349, 0.330, 0.315, 0.301, 0.288,
    0.276, 0.267, 0.259, 0.253, 0.250, 0.246, 0.244, 0.243, 0.243, 0.243, 0.242, 0.242, 0.245,
    0.254, 0.271, 0.301,
];

/// Amplitude de la fonction de transfert linéaire normalisée à 1 kHz, `L_U`.
const L_U: [f64; 29] = [
    -31.6, -27.2, -23.0, -19.1, -15.9, -13.0, -10.3, -8.1, -6.2, -4.5, -3.1, -2.0, -1.1, -0.4, 0.0,
    0.3, 0.5, 0.0, -2.7, -4.1, -1.0, 1.7, 2.5, 1.2, -2.1, -7.1, -11.2, -10.7, -3.1,
];

/// Seuil d'audition, `T_f`.
const T_F: [f64; 29] = [
    78.5, 68.7, 59.5, 51.1, 44.0, 37.5, 31.5, 26.5, 22.1, 17.9, 14.4, 11.4, 8.6, 6.2, 4.4, 3.0,
    2.2, 2.4, 3.5, 1.7, -1.3, -4.2, -6.0, -5.4, -1.5, 6.0, 12.6, 13.9, 12.3,
];

/// Indice de 1 kHz dans le tableau.
pub const INDICE_1_KHZ: usize = 17;

/// Indice d'une fréquence du tableau (comparaison exacte).
pub fn indice(f: f64) -> Option<usize> {
    FREQUENCES.iter().position(|&x| x == f)
}

/// `L_p` en dB SPL à la fréquence d'indice `i` pour `phon` phones.
pub fn niveau_spl(i: usize, phon: f64) -> f64 {
    let a_f = 4.47e-3 * (10f64.powf(0.025 * phon) - 1.15)
        + (0.4 * 10f64.powf((T_F[i] + L_U[i]) / 10.0 - 9.0)).powf(ALPHA_F[i]);
    10.0 / ALPHA_F[i] * a_f.log10() - L_U[i] + 94.0
}

/// La correction (dB, relative à 1 kHz) qui rend à une écoute à
/// `phon_ecoute` l'équilibre tonal perçu à `phon_reference`. Positive aux
/// basses fréquences quand on écoute moins fort que la référence.
pub fn compensation_db(i: usize, phon_ecoute: f64, phon_reference: f64) -> f64 {
    let delta = |j: usize| niveau_spl(j, phon_ecoute) - niveau_spl(j, phon_reference);
    delta(i) - delta(INDICE_1_KHZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valeurs_publiees_de_la_norme() {
        // À 1 kHz, L_p = L_N par définition.
        for phon in [20.0, 40.0, 60.0, 80.0] {
            assert!((niveau_spl(INDICE_1_KHZ, phon) - phon).abs() < 0.1);
        }
        // Courbe de 40 phones (ISO 226:2003) : 99,85 dB à 20 Hz, 64,4 dB à
        // 100 Hz ; 80 phones : 119,0 dB à 20 Hz.
        assert!((niveau_spl(0, 40.0) - 99.85).abs() < 0.1);
        assert!((niveau_spl(indice(100.0).unwrap(), 40.0) - 64.4).abs() < 0.1);
        assert!((niveau_spl(0, 80.0) - 119.0).abs() < 0.1);
    }

    #[test]
    fn compensation_nulle_a_la_reference_et_croissante_en_baissant() {
        let i40 = indice(40.0).unwrap();
        assert!(compensation_db(i40, 80.0, 80.0).abs() < 1e-12);
        let mut avant = 0.0;
        for v in [10.0, 20.0, 30.0, 40.0] {
            let c = compensation_db(i40, 80.0 - v, 80.0);
            assert!(c > avant, "{v} dB plus bas : {c}");
            avant = c;
        }
        // 30 dB sous 80 phones : +13,2 dB à 40 Hz.
        assert!((compensation_db(i40, 50.0, 80.0) - 13.15).abs() < 0.05);
    }
}
