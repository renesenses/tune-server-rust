//! Loi du curseur de volume (#1274, #6787).
//!
//! Le serveur publie et reçoit TOUJOURS un gain linéaire 0..1. MPRIS, lui,
//! publie ce que l'applet du bureau montre comme un curseur : sur une zone
//! `volume_curve: "db60"` (sortie logicielle), la position perceptive — 60 dB
//! de course, une droite sous le genou de 10 %. Mêmes formules que
//! `tune_core::audio::volume_scale` et que `tune-widget/src/contrat.js`.

const PLAGE_PERCEPTIVE_DB: f64 = 60.0;
const GENOU_PERCEPTIF: f64 = 0.1;

fn gain_au_genou() -> f64 {
    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - GENOU_PERCEPTIF) / 20.0)
}

/// La loi du curseur d'une zone (`volume_curve` de `GET /zones`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Courbe {
    #[default]
    Lineaire,
    Db60,
}

impl Courbe {
    pub fn depuis(valeur: Option<&str>) -> Self {
        if valeur == Some("db60") {
            Courbe::Db60
        } else {
            Courbe::Lineaire
        }
    }
}

fn gain_perceptif(position: f64) -> f64 {
    if position.is_nan() || position <= 0.0 {
        return 0.0;
    }
    if position >= 1.0 {
        return 1.0;
    }
    if position < GENOU_PERCEPTIF {
        return gain_au_genou() * position / GENOU_PERCEPTIF;
    }
    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - position) / 20.0)
}

fn position_perceptive(gain: f64) -> f64 {
    if gain.is_nan() || gain <= 0.0 {
        return 0.0;
    }
    if gain >= 1.0 {
        return 1.0;
    }
    let genou = gain_au_genou();
    if gain < genou {
        return (gain / genou * GENOU_PERCEPTIF).min(GENOU_PERCEPTIF);
    }
    (1.0 + 20.0 * gain.log10() / PLAGE_PERCEPTIVE_DB).clamp(GENOU_PERCEPTIF, 1.0)
}

/// Gain publié par le serveur → `Volume` MPRIS (0..1).
///
/// Un serveur d'avant #1274 pouvait publier 0..100 : ramené à 0..1.
pub fn position_du_gain(gain: f64, courbe: Courbe) -> f64 {
    let mut g = if gain.is_finite() && gain > 0.0 {
        gain
    } else {
        0.0
    };
    if g > 1.0 {
        g /= 100.0;
    }
    let g = g.min(1.0);
    match courbe {
        Courbe::Db60 => position_perceptive(g),
        Courbe::Lineaire => g,
    }
}

/// `Volume` MPRIS demandé (0..1) → gain à envoyer au serveur.
pub fn gain_de_position(position: f64, courbe: Courbe) -> f64 {
    let p = if position.is_finite() {
        position.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match courbe {
        Courbe::Db60 => gain_perceptif(p),
        Courbe::Lineaire => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proche(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn lineaire_est_l_identite_bornee() {
        assert!(proche(position_du_gain(0.42, Courbe::Lineaire), 0.42));
        assert!(proche(gain_de_position(0.42, Courbe::Lineaire), 0.42));
        assert!(proche(gain_de_position(1.7, Courbe::Lineaire), 1.0));
        assert!(proche(gain_de_position(-0.2, Courbe::Lineaire), 0.0));
        assert!(proche(gain_de_position(f64::NAN, Courbe::Lineaire), 0.0));
    }

    #[test]
    fn un_serveur_ancien_en_pourcent_est_ramene_a_un() {
        assert!(proche(position_du_gain(42.0, Courbe::Lineaire), 0.42));
    }

    #[test]
    fn db60_mi_course_vaut_moins_30_db() {
        // 50 % du curseur = -30 dB = gain 10^(-1.5).
        assert!(proche(
            gain_de_position(0.5, Courbe::Db60),
            10f64.powf(-1.5)
        ));
        assert!(proche(
            position_du_gain(10f64.powf(-1.5), Courbe::Db60),
            0.5
        ));
        assert!(proche(gain_de_position(1.0, Courbe::Db60), 1.0));
        assert!(proche(gain_de_position(0.0, Courbe::Db60), 0.0));
    }

    #[test]
    fn db60_aller_retour_sous_et_sur_le_genou() {
        for p in [0.01, 0.05, 0.099, 0.1, 0.25, 0.73, 0.99] {
            let retour = position_du_gain(gain_de_position(p, Courbe::Db60), Courbe::Db60);
            assert!((retour - p).abs() < 1e-9, "{p} → {retour}");
        }
    }

    #[test]
    fn la_courbe_se_lit_dans_volume_curve() {
        assert_eq!(Courbe::depuis(Some("db60")), Courbe::Db60);
        assert_eq!(Courbe::depuis(Some("linear")), Courbe::Lineaire);
        assert_eq!(Courbe::depuis(None), Courbe::Lineaire);
    }
}
