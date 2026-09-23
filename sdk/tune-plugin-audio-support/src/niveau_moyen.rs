//! #4685 — le niveau MOYEN qu'un filtre fait gagner ou perdre, pour le
//! compenser d'un gain FIXE.
//!
//! « À volume égal, on juge le réglage, pas le fait que plus fort sonne
//! mieux » : l'égaliseur (réserve anti-écrêtage) et le crossfeed (qui creuse
//! la différence gauche/droite, surtout dans le grave) changent le niveau
//! moyen. La compensation retenue n'est PAS un automatisme qui suivrait la
//! musique — il ferait « pomper » le son d'un morceau à l'autre — mais un
//! nombre calculé une fois depuis la réponse du filtre, identique pour toute
//! la musique.
//!
//! La référence est un **bruit rose** sur la bande audible : même énergie par
//! octave, c'est-à-dire une densité uniforme sur une échelle LOGARITHMIQUE des
//! fréquences. D'où la moyenne sur une grille log-uniforme, sans autre
//! pondération. Ce n'est pas une sonie (ni ISO 226, ni LUFS) : c'est la
//! moyenne de puissance d'un signal large bande de pente musicale, et elle se
//! vérifie au RMS sur un multi-sinus de même répartition (voir les témoins
//! des deux greffons).
//!
//! Une seule implémentation, partagée par l'égaliseur et le crossfeed : deux
//! grilles écrites à deux endroits finiraient par ne plus répondre à la même
//! question.

/// Bas de la bande de référence, en Hz.
pub const BANDE_BASSE_HZ: f64 = 20.0;
/// Haut de la bande de référence, en Hz — ramené sous Nyquist au besoin.
pub const BANDE_HAUTE_HZ: f64 = 20_000.0;

/// Points de la grille log-uniforme. 512 points sur dix octaves, c'est
/// ~51 points par octave : bien plus fin que la plus étroite des cloches de
/// l'égaliseur (Q = 30, ~1/20 d'octave) n'en demande pour une MOYENNE.
const POINTS: usize = 512;

/// La grille de fréquences de la référence, en Hz, pour ce débit.
///
/// Vide quand le débit ne laisse aucune bande (débit nul, non fini, ou
/// Nyquist sous 20 Hz).
pub fn grille_rose(sample_rate: f64) -> Vec<f64> {
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Vec::new();
    }
    let haute = BANDE_HAUTE_HZ.min(sample_rate * 0.499);
    if haute <= BANDE_BASSE_HZ {
        return Vec::new();
    }
    (0..POINTS)
        .map(|i| BANDE_BASSE_HZ * (haute / BANDE_BASSE_HZ).powf(i as f64 / (POINTS - 1) as f64))
        .collect()
}

/// Gain de puissance MOYEN d'un filtre sur un bruit rose, en dB.
///
/// `puissance(f)` rend |H(f)|² — le gain de PUISSANCE du filtre à la
/// fréquence `f` en Hz, pas son module. Une valeur non finie ou négative est
/// ignorée plutôt que propagée : un point absurde ne doit pas faire d'une
/// compensation un `NaN` qui atteindrait le volume.
///
/// Rend 0,0 (aucune compensation) quand rien n'est mesurable.
pub fn gain_moyen_rose_db(sample_rate: f64, puissance: impl Fn(f64) -> f64) -> f64 {
    let (somme, n) = grille_rose(sample_rate)
        .into_iter()
        .map(puissance)
        .filter(|p| p.is_finite() && *p >= 0.0)
        .fold((0.0_f64, 0_usize), |(s, n), p| (s + p, n + 1));
    if n == 0 || somme <= 0.0 {
        return 0.0;
    }
    10.0 * (somme / n as f64).log10()
}

/// Le signal CONNU qui vérifie [`gain_moyen_rose_db`] au RMS : une somme de
/// sinus d'amplitudes égales, un par douzième d'octave de 20 Hz à 20 kHz (sous
/// Nyquist), de phases pseudo-aléatoires tirées de `graine`.
///
/// Même répartition d'énergie que la référence — autant par octave — donc
/// le RMS d'un filtre mesuré sur ce signal doit retrouver la moyenne
/// calculée, à la finesse de la grille près. Deux graines différentes
/// donnent deux signaux décorrélés (phases indépendantes) : c'est ce qui
/// permet aux témoins du crossfeed de bâtir un Mid et un Side.
///
/// Normalisé en crête à 0,25 : assez loin du rail pour qu'aucun étage ne
/// l'écrête, ce qui fausserait une mesure de moyenne.
pub fn multisinus_rose(sample_rate: u32, frames: usize, graine: u64) -> Vec<f64> {
    let fs = f64::from(sample_rate);
    let haute = BANDE_HAUTE_HZ.min(fs * 0.45);
    let mut etat = graine | 1;
    let mut hasard = move || {
        // xorshift64 : déterministe, sans dépendance.
        etat ^= etat << 13;
        etat ^= etat >> 7;
        etat ^= etat << 17;
        (etat >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut raies = Vec::new();
    let mut f = BANDE_BASSE_HZ;
    while f <= haute {
        raies.push((f, 2.0 * std::f64::consts::PI * hasard()));
        f *= 2.0_f64.powf(1.0 / 12.0);
    }
    let mut signal: Vec<f64> = (0..frames)
        .map(|n| {
            let t = n as f64 / fs;
            raies
                .iter()
                .map(|(f, phase)| (2.0 * std::f64::consts::PI * f * t + phase).sin())
                .sum()
        })
        .collect();
    let crete = signal.iter().fold(0.0_f64, |m, s| m.max(s.abs()));
    if crete > 0.0 {
        signal.iter_mut().for_each(|s| *s *= 0.25 / crete);
    }
    signal
}

/// RMS d'une suite d'échantillons, en dB (plein échelle = 0).
pub fn rms_db(echantillons: impl IntoIterator<Item = f64>) -> f64 {
    let (somme, n) = echantillons
        .into_iter()
        .fold((0.0_f64, 0_usize), |(s, n), x| (s + x * x, n + 1));
    if n == 0 || somme <= 0.0 {
        return f64::NEG_INFINITY;
    }
    10.0 * (somme / n as f64).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_filtre_neutre_rend_zero() {
        assert!(gain_moyen_rose_db(44_100.0, |_| 1.0).abs() < 1e-12);
    }

    #[test]
    fn un_gain_plat_rend_son_propre_niveau() {
        // |H|² = 0,25, soit −6,02 dB partout.
        let g = gain_moyen_rose_db(48_000.0, |_| 0.25);
        assert!((g - (-6.0206)).abs() < 1e-3, "{g}");
    }

    /// La pondération est ROSE : une octave pèse autant qu'une autre. Un
    /// filtre qui coupe tout sous 200 Hz (une décade sur trois) doit perdre
    /// un tiers de la puissance, pas 1 % comme le ferait une moyenne linéaire
    /// en fréquence.
    #[test]
    fn la_ponderation_est_logarithmique() {
        let g = gain_moyen_rose_db(40_200.0, |f| if f < 200.0 { 0.0 } else { 1.0 });
        let attendu = 10.0 * (2.0_f64 / 3.0).log10();
        assert!((g - attendu).abs() < 0.02, "{g} vs {attendu}");
    }

    #[test]
    fn un_debit_absurde_ne_compense_rien() {
        assert_eq!(gain_moyen_rose_db(0.0, |_| 4.0), 0.0);
        assert_eq!(gain_moyen_rose_db(f64::NAN, |_| 4.0), 0.0);
        assert_eq!(gain_moyen_rose_db(44_100.0, |_| f64::NAN), 0.0);
    }
}
