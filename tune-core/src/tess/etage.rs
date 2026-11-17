//! Le trait d'un étage TESS.
//!
//! Un étage reçoit un tampon d'échantillons **entrelacés en `f64`** et le
//! transforme sur place. Aucune conversion `f32` ni entière n'a lieu entre deux
//! étages (décision Q5) : la seule quantification est l'étape finale de la
//! chaîne ([`crate::tess::CodeEtape::Quantization`] ou le modulateur DSD).
//!
//! Le tampon est un `Vec` et non une tranche : un étage déclaré
//! [`crate::tess::Canaux::Remaps`] (réaffectation des canaux) change le nombre
//! de canaux, donc la longueur du tampon. Les autres étages n'en changent ni la
//! longueur ni le nombre de canaux.

use serde::{Deserialize, Serialize};

/// Le format du flux à l'entrée ou à la sortie d'un étage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FormatFlux {
    pub channels: u16,
    pub sample_rate_hz: u32,
}

/// Un bloc d'échantillons entrelacés en `f64`, pleine échelle à ±1,0.
#[derive(Debug, Clone, PartialEq)]
pub struct TamponTess {
    pub echantillons: Vec<f64>,
    pub canaux: u16,
    pub frequence_hz: u32,
}

impl TamponTess {
    pub fn new(echantillons: Vec<f64>, canaux: u16, frequence_hz: u32) -> Self {
        Self {
            echantillons,
            canaux,
            frequence_hz,
        }
    }

    pub fn format(&self) -> FormatFlux {
        FormatFlux {
            channels: self.canaux,
            sample_rate_hz: self.frequence_hz,
        }
    }

    /// Nombre de trames (un échantillon par canal). Zéro canal = zéro trame.
    pub fn trames(&self) -> usize {
        if self.canaux == 0 {
            0
        } else {
            self.echantillons.len() / usize::from(self.canaux)
        }
    }

    /// Le tampon contient-il un nombre entier de trames ?
    pub fn coherent(&self) -> bool {
        self.canaux > 0
            && self
                .echantillons
                .len()
                .is_multiple_of(usize::from(self.canaux))
    }
}

/// Pourquoi un étage refuse de travailler.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurEtage {
    #[error("format non pris en charge : {channels} canaux à {sample_rate_hz} Hz")]
    FormatNonPrisEnCharge { channels: u16, sample_rate_hz: u32 },
    #[error("tampon incohérent : {longueur} échantillons pour {canaux} canaux")]
    TamponIncoherent { longueur: usize, canaux: u16 },
    #[error("réglages refusés : {0}")]
    Reglages(String),
}

/// Un étage de traitement TESS (greffon ou étage fixe).
///
/// Cycle de vie attendu par l'exécuteur (lot T03) :
/// 1. [`preparer`](EtageTess::preparer) à chaque changement de format ; l'étage
///    rend le format de sa sortie (identique à l'entrée, sauf réaffectation des
///    canaux ou changement de cadence) ;
/// 2. [`traiter`](EtageTess::traiter) bloc après bloc ;
/// 3. [`reinitialiser`](EtageTess::reinitialiser) sur un saut (seek, piste
///    suivante non gapless) pour vider les mémoires internes.
///
/// En `bypass`, un étage NE TOUCHE PAS le tampon : c'est ce qui garde la
/// chaîne vide ou contournée bit-perfect.
pub trait EtageTess: Send {
    /// Identifiant stable : celui du [`crate::tess::DescripteurGreffon`] pour un
    /// greffon, le code de l'étape pour un étage fixe.
    fn id(&self) -> &str;

    /// Prépare l'étage pour un format d'entrée et rend le format de sortie.
    fn preparer(&mut self, entree: FormatFlux) -> Result<FormatFlux, ErreurEtage>;

    /// Traite un bloc sur place.
    fn traiter(&mut self, tampon: &mut TamponTess) -> Result<(), ErreurEtage>;

    /// Latence introduite, en trames à la fréquence de sortie (anticipation
    /// d'un déclic, convolution partitionnée…). Annoncée à l'hôte pour
    /// l'alignement multi-points et le chemin du signal.
    fn latence_trames(&self) -> u32 {
        0
    }

    fn bypass(&self) -> bool;

    fn definir_bypass(&mut self, bypass: bool);

    /// Vide les mémoires internes (filtres, lignes à retard).
    fn reinitialiser(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    /// Un étage minimal : un gain linéaire.
    struct Gain {
        facteur: f64,
        contourne: bool,
    }

    impl EtageTess for Gain {
        fn id(&self) -> &str {
            "gain-de-test"
        }
        fn preparer(&mut self, entree: FormatFlux) -> Result<FormatFlux, ErreurEtage> {
            if entree.channels == 0 {
                return Err(ErreurEtage::FormatNonPrisEnCharge {
                    channels: entree.channels,
                    sample_rate_hz: entree.sample_rate_hz,
                });
            }
            Ok(entree)
        }
        fn traiter(&mut self, tampon: &mut TamponTess) -> Result<(), ErreurEtage> {
            if !tampon.coherent() {
                return Err(ErreurEtage::TamponIncoherent {
                    longueur: tampon.echantillons.len(),
                    canaux: tampon.canaux,
                });
            }
            if self.contourne {
                return Ok(());
            }
            for e in &mut tampon.echantillons {
                *e *= self.facteur;
            }
            Ok(())
        }
        fn bypass(&self) -> bool {
            self.contourne
        }
        fn definir_bypass(&mut self, bypass: bool) {
            self.contourne = bypass;
        }
    }

    fn type_des_echantillons<T: 'static>(_: &[T]) -> TypeId {
        TypeId::of::<T>()
    }

    #[test]
    fn le_tampon_d_un_etage_est_en_f64() {
        // Preuve par le TYPE, pas par une lecture de code (critère « 64 bits réels »).
        let t = TamponTess::new(vec![0.0; 4], 2, 96_000);
        assert_eq!(type_des_echantillons(&t.echantillons), TypeId::of::<f64>());
        assert_ne!(type_des_echantillons(&t.echantillons), TypeId::of::<f32>());
    }

    #[test]
    fn deux_etages_en_f64_conservent_la_double_precision() {
        // 1 + 2^-40 : représentable en f64, pas en f32 (24 bits de mantisse).
        let x = 1.0 + 2f64.powi(-40);
        let mut t = TamponTess::new(vec![x, -x], 2, 48_000);
        let mut moitie = Gain {
            facteur: 0.5,
            contourne: false,
        };
        let mut double = Gain {
            facteur: 2.0,
            contourne: false,
        };
        moitie.traiter(&mut t).unwrap();
        double.traiter(&mut t).unwrap();
        assert_eq!(t.echantillons, vec![x, -x]);
        // Contre-épreuve : le même aller-retour par un tampon f32 perd l'écart.
        let par_f32 = ((x as f32) * 0.5 * 2.0) as f64;
        assert_ne!(par_f32, x);
    }

    #[test]
    fn un_etage_contourne_ne_touche_pas_le_tampon() {
        let source = vec![0.1, -0.2, 0.3, -0.4];
        let mut t = TamponTess::new(source.clone(), 2, 44_100);
        let mut g = Gain {
            facteur: 0.25,
            contourne: false,
        };
        g.definir_bypass(true);
        assert!(g.bypass());
        g.traiter(&mut t).unwrap();
        assert_eq!(t.echantillons, source);
        // Contre-épreuve : actif, il le modifie.
        g.definir_bypass(false);
        g.traiter(&mut t).unwrap();
        assert_ne!(t.echantillons, source);
    }

    #[test]
    fn un_tampon_incoherent_est_refuse() {
        let mut t = TamponTess::new(vec![0.0; 3], 2, 44_100);
        assert!(!t.coherent());
        assert_eq!(t.trames(), 1);
        let mut g = Gain {
            facteur: 1.0,
            contourne: false,
        };
        assert_eq!(
            g.traiter(&mut t),
            Err(ErreurEtage::TamponIncoherent {
                longueur: 3,
                canaux: 2
            })
        );
        assert_eq!(g.latence_trames(), 0);
        assert_eq!(
            g.preparer(FormatFlux {
                channels: 2,
                sample_rate_hz: 96_000
            }),
            Ok(t.format().with_rate(96_000))
        );
    }

    impl FormatFlux {
        fn with_rate(mut self, r: u32) -> Self {
            self.sample_rate_hz = r;
            self
        }
    }
}
