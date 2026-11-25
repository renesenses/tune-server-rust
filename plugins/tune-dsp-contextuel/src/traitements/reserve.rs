//! #8 — Anti-écrêtage inter-échantillons : une CONTRAINTE, pas un étage.
//!
//! TESS a déjà UNE autorité de marge (headroom automatique ou manuel, #9520,
//! #9546). Un second étage de réserve s'y ADDITIONNERAIT (le cas de #9518 ×
//! #9520). Cette caisse ne pose donc aucun gain : elle exporte ce qu'elle
//! sait — la crête vraie mesurée (BS.1770, `tune_core::audio::crete_vraie`)
//! et le gain positif maximal de ses étages (loudness) — dans une
//! [`ContrainteDeMarge`], que l'autorité de TESS combine avec ses propres
//! exigences en prenant le MAXIMUM, jamais la somme.

use serde::{Deserialize, Serialize};

pub const MARGE_PAR_DEFAUT_DB: f64 = 0.3;
/// Au-delà, la mesure est suspecte : la réserve demandée est bornée.
pub const RESERVE_MAX_DB: f64 = 6.0;

/// La réserve qu'appelle une crête vraie : `max(0, crête + marge)`, bornée à
/// [`RESERVE_MAX_DB`]. Une crête inconnue ou non finie n'appelle rien.
pub fn reserve_pour_la_crete(crete_vraie_dbtp: Option<f64>, marge_db: f64) -> f64 {
    let marge = if marge_db.is_finite() {
        marge_db.clamp(0.0, 3.0)
    } else {
        MARGE_PAR_DEFAUT_DB
    };
    match crete_vraie_dbtp {
        Some(c) if c.is_finite() => (c + marge).clamp(0.0, RESERVE_MAX_DB),
        _ => 0.0,
    }
}

/// Ce que les profils contextuels d'un album demandent à l'autorité de marge
/// de TESS.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ContrainteDeMarge {
    /// Crête vraie mesurée de l'album, si l'anti-écrêtage est actif.
    pub crete_vraie_dbtp: Option<f64>,
    /// Réserve qu'appelle cette crête (dB, ≥ 0).
    pub reserve_crete_db: f64,
    /// Plus fort gain positif que les étages contextuels actifs peuvent
    /// apporter, à toute fréquence et tout volume (dB, ≥ 0) : le plafond du
    /// loudness.
    pub gain_positif_max_db: f64,
}

impl ContrainteDeMarge {
    /// La marge totale à garantir avant les étages contextuels (dB, ≥ 0) :
    /// crête et gain positif s'ajoutent, car le loudness peut monter un
    /// master déjà au-dessus de 0 dBTP. L'autorité de TESS prend ensuite le
    /// MAXIMUM de cette valeur et de sa propre marge.
    pub fn marge_requise_db(&self) -> f64 {
        self.reserve_crete_db + self.gain_positif_max_db
    }

    pub fn est_vide(&self) -> bool {
        self.marge_requise_db() == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::super::lineaire;
    use super::*;
    use tune_core::audio::crete_vraie::CreteVraie;

    #[test]
    fn reserve_calee_sur_la_crete_vraie() {
        assert!((reserve_pour_la_crete(Some(1.2), 0.3) - 1.5).abs() < 1e-12);
        assert_eq!(reserve_pour_la_crete(Some(-2.0), 0.3), 0.0);
        assert_eq!(reserve_pour_la_crete(Some(12.0), 0.3), RESERVE_MAX_DB);
        assert_eq!(reserve_pour_la_crete(None, 0.3), 0.0);
        assert_eq!(reserve_pour_la_crete(Some(f64::NAN), 0.3), 0.0);
    }

    #[test]
    fn la_reserve_exportee_supprime_l_over_inter_echantillons() {
        // fs/4 déphasé de 45° : échantillons à ±0,707·A, crête vraie A.
        // A = 1,3 : crête d'échantillon −0,75 dBFS, crête vraie ≈ +2,3 dBTP.
        let fs = 44_100usize;
        let a = 1.3;
        let mut entree = Vec::with_capacity(fs * 2);
        for n in 0..fs {
            let x =
                a * (std::f64::consts::FRAC_PI_2 * n as f64 + std::f64::consts::FRAC_PI_4).sin();
            entree.extend([x, x]);
        }
        let mut cv = CreteVraie::new(fs, 2);
        cv.nourrir(&entree);
        let crete_dbtp = 20.0 * cv.crete().log10();
        assert!(crete_dbtp > 2.0, "crête vraie {crete_dbtp}");
        // L'autorité de TESS applique la réserve demandée.
        let g = lineaire(-reserve_pour_la_crete(
            Some(crete_dbtp),
            MARGE_PAR_DEFAUT_DB,
        ));
        let sortie: Vec<f64> = entree.iter().map(|x| x * g).collect();
        let mut apres = CreteVraie::new(fs, 2);
        apres.nourrir(&sortie);
        let db = 20.0 * apres.crete().log10();
        assert!(
            db <= -MARGE_PAR_DEFAUT_DB + 0.01,
            "crête vraie après : {db} dBTP"
        );
    }

    #[test]
    fn marge_requise_additionne_crete_et_gain_positif() {
        let c = ContrainteDeMarge {
            crete_vraie_dbtp: Some(0.9),
            reserve_crete_db: 1.2,
            gain_positif_max_db: 3.0,
        };
        assert!((c.marge_requise_db() - 4.2).abs() < 1e-12);
        assert!(ContrainteDeMarge::default().est_vide());
    }
}
