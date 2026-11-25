//! L'interface minimale vers TESS : un [`Traitement`] habillé en
//! `tune_core::tess::etage::EtageTess` (sur place, latence nulle), et la
//! construction des étages ACTIFS d'un profil résolu.
//!
//! Rien ici ne modifie TESS : le branchement (insertion des étages dans la
//! chaîne d'une zone, à la résolution de chaque album) se fera après la
//! fusion des lots TESS en cours.

use tune_core::tess::etage::{Disposition, ErreurEtage, EtageTess, FormatFlux, verifier_tampon};

use crate::profil::{Etat, ProfilResolu};
use crate::traitements::{ErreurTraitement, OrigineDsd, Traitement};

/// Préfixe des identifiants d'étage : `dsp-contextuel.<code du genre>`.
pub const PREFIXE_ID: &str = "dsp-contextuel.";

pub struct EtageContextuel {
    id: String,
    traitement: Box<dyn Traitement>,
    canaux: u16,
    max_trames: usize,
    bypass: bool,
}

impl EtageContextuel {
    pub fn new(traitement: Box<dyn Traitement>) -> Self {
        Self {
            id: format!("{PREFIXE_ID}{}", traitement.genre().code()),
            traitement,
            canaux: 0,
            max_trames: 0,
            bypass: true,
        }
    }

    pub fn traitement_mut(&mut self) -> &mut dyn Traitement {
        self.traitement.as_mut()
    }
}

impl EtageTess for EtageContextuel {
    fn id(&self) -> &str {
        &self.id
    }

    fn preparer(
        &mut self,
        entree: FormatFlux,
        max_trames: usize,
    ) -> Result<Disposition, ErreurEtage> {
        self.traitement
            .preparer(entree.channels, entree.sample_rate_hz)
            .map_err(|e| match e {
                ErreurTraitement::FormatNonPrisEnCharge { .. } => {
                    ErreurEtage::FormatNonPrisEnCharge {
                        channels: entree.channels,
                        sample_rate_hz: entree.sample_rate_hz,
                    }
                }
                ErreurTraitement::OrigineNonPcm => {
                    ErreurEtage::Reglages("filtre ultrasonique : DSD converti en PCM seulement")
                }
            })?;
        self.canaux = entree.channels;
        self.max_trames = max_trames;
        Ok(Disposition::sur_place(entree, max_trames, 0))
    }

    fn traiter_sur_place(&mut self, tampon: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        if self.canaux == 0 {
            return Err(ErreurEtage::NonPrepare);
        }
        if trames > self.max_trames {
            return Err(ErreurEtage::BlocTropGrand {
                trames,
                max: self.max_trames,
            });
        }
        verifier_tampon(tampon.len(), trames, self.canaux)?;
        self.traitement.traiter(tampon, trames);
        Ok(())
    }

    fn reinitialiser(&mut self) {
        self.traitement.reinitialiser();
    }

    fn bypass(&self) -> bool {
        self.bypass
    }

    /// Latence nulle : le contournement est un fondu de `RAMPE_MS`, sans
    /// retard à conserver.
    fn definir_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
        self.traitement.activer(!bypass);
    }
}

/// Les étages des traitements ACTIFS d'un profil résolu, dans l'ordre
/// canonique, actifs d'emblée (pas de fondu d'entrée : début de flux). Un
/// profil sans traitement actif rend une liste vide : la chaîne reste
/// bit-perfect.
pub fn etages_actifs(resolu: &ProfilResolu, origine: OrigineDsd) -> Vec<EtageContextuel> {
    resolu
        .traitements
        .iter()
        .filter(|t| t.etat == Etat::Actif)
        .map(|t| {
            let mut tr = t.reglages.construire(origine);
            tr.activer(true);
            let mut e = EtageContextuel::new(tr);
            e.bypass = false;
            e
        })
        .collect()
}

/// À appeler APRÈS `preparer` pour démarrer sans fondu d'entrée.
pub fn demarrer_sans_fondu(etages: &mut [EtageContextuel]) {
    for e in etages {
        e.traitement.sauter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profil::{ContexteAlbum, SourcePreaccentuation, profil_vague_1, resoudre};
    use crate::traitements::essais::bruit;

    #[test]
    fn profil_vague_1_sur_un_cd_preaccentue_donne_un_etage() {
        let c = ContexteAlbum {
            preaccentuation: Some(SourcePreaccentuation::Cue),
            ..Default::default()
        };
        let r = resoudre(&profil_vague_1(), &c, &[]);
        let mut etages = etages_actifs(&r, OrigineDsd::Aucune);
        assert_eq!(etages.len(), 1);
        assert_eq!(etages[0].id(), "dsp-contextuel.desaccentuation");
        let f = FormatFlux {
            channels: 2,
            sample_rate_hz: 44_100,
        };
        let d = etages[0].preparer(f, 1_024).unwrap();
        assert!(d.sur_place);
        assert_eq!(d.latence_trames, 0);
        demarrer_sans_fondu(&mut etages);
        let mut b = bruit(1_024, 2, 0.5, 1);
        let avant = b.clone();
        etages[0].traiter_sur_place(&mut b, 1_024).unwrap();
        assert_ne!(b, avant);
        assert_eq!(
            etages[0].traiter_sur_place(&mut b, 2_048),
            Err(ErreurEtage::BlocTropGrand {
                trames: 2_048,
                max: 1_024
            })
        );
    }

    #[test]
    fn sans_traitement_actif_aucun_etage() {
        let r = resoudre(&profil_vague_1(), &ContexteAlbum::default(), &[]);
        assert!(etages_actifs(&r, OrigineDsd::Aucune).is_empty());
    }

    #[test]
    fn bypass_rend_l_entree_au_bit_pres_apres_le_fondu() {
        let c = ContexteAlbum {
            preaccentuation: Some(SourcePreaccentuation::Toc),
            ..Default::default()
        };
        let r = resoudre(&profil_vague_1(), &c, &[]);
        let mut e = etages_actifs(&r, OrigineDsd::Aucune).remove(0);
        e.preparer(
            FormatFlux {
                channels: 2,
                sample_rate_hz: 48_000,
            },
            4_096,
        )
        .unwrap();
        e.definir_bypass(true);
        let mut b = bruit(4_096, 2, 0.5, 3);
        e.traiter_sur_place(&mut b, 4_096).unwrap(); // fondu de 960 trames
        let entree = bruit(4_096, 2, 0.5, 4);
        let mut b = entree.clone();
        e.traiter_sur_place(&mut b, 4_096).unwrap();
        assert_eq!(b, entree);
    }

    #[test]
    fn ultrasonique_refuse_par_tess_hors_dsd_converti() {
        let mut e = EtageContextuel::new(
            crate::traitements::Reglages::par_defaut(
                crate::traitements::GenreTraitement::UltrasoniqueDsd,
            )
            .construire(OrigineDsd::Dop),
        );
        let f = FormatFlux {
            channels: 2,
            sample_rate_hz: 176_400,
        };
        assert!(matches!(e.preparer(f, 512), Err(ErreurEtage::Reglages(_))));
    }
}
