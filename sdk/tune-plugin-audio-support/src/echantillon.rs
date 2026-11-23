//! TESS T06 — le type d'échantillon des moteurs de référence.
//!
//! Les moteurs historiques (égaliseur, crossfeed) recevaient des `f32` et
//! calculaient, selon le moteur, en `f64` (biquads) ou en `f32` (crossfeed).
//! L'exécuteur TESS tient le signal en `f64` de bout en bout (Q5) : chaque
//! moteur reçoit désormais un `&mut [f64]` sans passer par un `f32`.
//!
//! Plutôt que de dupliquer les boucles, elles sont écrites UNE fois pour
//! `T: Echantillon`, avec deux implémentations :
//!
//! - **`f32`** : exactement les opérations d'avant. Les valeurs lues d'un
//!   `f32` et rangées en `f64` (lignes à retard) y reviennent EXACTES
//!   ([`Echantillon::depuis_f64`] d'une valeur qui vient d'un `f32` est
//!   l'identité) : le chemin `f32` est donc identique au bit près à l'ancien,
//!   ce que les témoins historiques vérifient.
//! - **`f64`** : les mêmes opérations, en double précision, sans aucun
//!   arrondi intermédiaire vers `f32`.
use core::ops::{Add, Mul, Sub};

/// Un échantillon flottant, pleine échelle ±1,0, que les moteurs savent
/// traiter.
pub trait Echantillon:
    Copy + PartialOrd + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Send + 'static
{
    /// Zéro.
    const ZERO: Self;
    /// Vrai pour `f64` : le moteur n'a aucune frontière de précision.
    const DOUBLE: bool;

    /// Vers le domaine de calcul `f64`, exactement.
    fn vers_f64(self) -> f64;
    /// Depuis le domaine `f64` : arrondi au plus proche pour `f32`, identité
    /// pour `f64`.
    fn depuis_f64(x: f64) -> Self;
    /// Un coefficient `f32` (gain, force) dans ce type : exact pour les deux.
    fn depuis_f32(x: f32) -> Self;
    /// Borne à `[-1, 1]` (le garde-fou du crossfeed avant la sortie).
    fn borner_unite(self) -> Self;
}

impl Echantillon for f32 {
    const ZERO: Self = 0.0;
    const DOUBLE: bool = false;

    #[inline(always)]
    fn vers_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline(always)]
    fn depuis_f64(x: f64) -> Self {
        x as f32
    }
    #[inline(always)]
    fn depuis_f32(x: f32) -> Self {
        x
    }
    #[inline(always)]
    fn borner_unite(self) -> Self {
        self.clamp(-1.0, 1.0)
    }
}

impl Echantillon for f64 {
    const ZERO: Self = 0.0;
    const DOUBLE: bool = true;

    #[inline(always)]
    fn vers_f64(self) -> f64 {
        self
    }
    #[inline(always)]
    fn depuis_f64(x: f64) -> Self {
        x
    }
    #[inline(always)]
    fn depuis_f32(x: f32) -> Self {
        f64::from(x)
    }
    #[inline(always)]
    fn borner_unite(self) -> Self {
        self.clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un `f32` rangé en `f64` revient exact : c'est ce qui garde le chemin
    /// `f32` des moteurs identique au bit près.
    #[test]
    fn un_f32_range_en_f64_revient_exact() {
        for x in [0.0_f32, -0.0, 1.0, -1.0, 1e-38, 0.123_456_79, f32::MIN_POSITIVE] {
            assert_eq!(f32::depuis_f64(x.vers_f64()).to_bits(), x.to_bits());
        }
    }

    #[test]
    fn f64_ne_perd_rien() {
        let x = 0.1_f64 + 1e-12;
        assert_eq!(f64::depuis_f64(x), x);
        assert_ne!(f64::from(f32::depuis_f64(x)), x);
    }
}
