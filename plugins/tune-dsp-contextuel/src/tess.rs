//! L'interface minimale vers TESS : un [`Traitement`] habillé en
//! `tune_core::tess::etage::EtageTess` (sur place, latence nulle), et la
//! construction des étages ACTIFS d'un profil résolu.
//!
//! **Hausses** : toute bascule passe par [`EtageContextuel::definir_bypass`],
//! qui demande au traitement la hausse qu'elle causerait et la confie AVANT
//! à la [`PorteDesHausses`] (au branchement : la borne de remontée de
//! #9547). Les baisses gardent la rampe de 20 ms.
//!
//! **Marge** : la réserve n'est pas un étage ; la
//! [`crate::traitements::reserve::ContrainteDeMarge`] du profil résolu
//! (`ProfilResolu::contrainte_de_marge`) va à l'autorité de marge de TESS.
//!
//! Rien ici ne modifie TESS : le branchement (insertion des étages dans la
//! chaîne d'une zone, à la résolution de chaque album) se fera après la
//! fusion des lots TESS en cours.

use std::sync::Arc;

use tune_core::tess::etage::{Disposition, ErreurEtage, EtageTess, FormatFlux, verifier_tampon};

use crate::hausses::PorteDesHausses;
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
    porte: Arc<dyn PorteDesHausses>,
}

impl EtageContextuel {
    /// Un étage CONTOURNÉ (inactif). La porte est obligatoire : aucune
    /// hausse ne peut passer sans elle.
    pub fn new(traitement: Box<dyn Traitement>, porte: Arc<dyn PorteDesHausses>) -> Self {
        Self {
            id: format!("{PREFIXE_ID}{}", traitement.genre().code()),
            traitement,
            canaux: 0,
            max_trames: 0,
            bypass: true,
            porte,
        }
    }

    pub fn traitement(&self) -> &dyn Traitement {
        self.traitement.as_ref()
    }

    /// HORS TEMPS RÉEL : volume LOGICIEL de la zone (loudness). Baisser le
    /// volume ne fait jamais monter le niveau net (voir `loudness`).
    pub fn definir_volume_db(&mut self, volume_db: f64) {
        self.traitement.definir_volume_db(volume_db);
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
    /// retard à conserver. Une bascule qui ferait MONTER le niveau confie
    /// d'abord sa hausse à la porte.
    fn definir_bypass(&mut self, bypass: bool) {
        let hausse = self.traitement.hausse_si(!bypass);
        if hausse > 0.0 {
            self.porte.retenir_la_hausse_de(hausse);
        }
        self.bypass = bypass;
        self.traitement.activer(!bypass);
    }
}

/// Les étages des traitements ACTIFS d'un profil résolu, dans l'ordre
/// canonique, actifs d'emblée (pas de fondu d'entrée : début de flux). Un
/// profil sans traitement actif rend une liste vide : la chaîne reste
/// bit-perfect.
///
/// La réserve n'y figure jamais (contrainte, pas étage), ni un traitement
/// `Neutre`.
pub fn etages_actifs(
    resolu: &ProfilResolu,
    origine: OrigineDsd,
    porte: &Arc<dyn PorteDesHausses>,
) -> Vec<EtageContextuel> {
    resolu
        .traitements
        .iter()
        .filter(|t| t.etat == Etat::Actif)
        .filter_map(|t| t.reglages.construire(origine))
        .map(|mut tr| {
            tr.activer(true);
            let mut e = EtageContextuel::new(tr, Arc::clone(porte));
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
    use crate::hausses::PorteEnregistreuse;
    use crate::profil::{
        ChoixUtilisateur, ContexteAlbum, Decision, Portee, SourceDuVolume, SourcePreaccentuation,
        profil_vague_1, resoudre,
    };
    use crate::traitements::GenreTraitement;
    use crate::traitements::essais::bruit;

    fn porte() -> (Arc<PorteEnregistreuse>, Arc<dyn PorteDesHausses>) {
        let p = Arc::new(PorteEnregistreuse::default());
        let d: Arc<dyn PorteDesHausses> = p.clone();
        (p, d)
    }

    fn format(cadence: u32) -> FormatFlux {
        FormatFlux {
            channels: 2,
            sample_rate_hz: cadence,
        }
    }

    fn cd_preaccentue() -> ContexteAlbum {
        ContexteAlbum {
            preaccentuation: Some(SourcePreaccentuation::Toc),
            ..Default::default()
        }
    }

    #[test]
    fn profil_vague_1_sur_un_cd_preaccentue_donne_un_etage() {
        let r = resoudre(&profil_vague_1(), &cd_preaccentue(), &[]);
        let (_, p) = porte();
        let mut etages = etages_actifs(&r, OrigineDsd::Aucune, &p);
        assert_eq!(etages.len(), 1);
        assert_eq!(etages[0].id(), "dsp-contextuel.desaccentuation");
        let d = etages[0].preparer(format(44_100), 1_024).unwrap();
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
        let (_, p) = porte();
        assert!(etages_actifs(&r, OrigineDsd::Aucune, &p).is_empty());
    }

    /// Couper la désaccentuation rend les aigus : la hausse (≈ 9,5 dB à
    /// 20 kHz sous 44,1 kHz) passe par la porte AVANT la bascule ; la
    /// rallumer est une baisse, qui n'y passe pas.
    #[test]
    fn couper_confie_la_hausse_a_la_porte_rallumer_non() {
        let r = resoudre(&profil_vague_1(), &cd_preaccentue(), &[]);
        let (trace, p) = porte();
        let mut e = etages_actifs(&r, OrigineDsd::Aucune, &p).remove(0);
        e.preparer(format(44_100), 4_096).unwrap();
        e.definir_bypass(true);
        let h = trace.hausses();
        assert_eq!(h.len(), 1);
        assert!(h[0] > 9.0 && h[0] < 10.46, "hausse {}", h[0]);
        e.definir_bypass(false);
        assert_eq!(trace.hausses().len(), 1, "rallumer est une baisse");
    }

    #[test]
    fn equilibre_et_loudness_passent_par_la_porte() {
        let mut c = cd_preaccentue();
        c.volume = SourceDuVolume::Logiciel;
        c.mesures = Some(crate::analyse::Analyseur::new(44_100, 2).resultat());
        c.mesures.as_mut().unwrap().ecart_lr_db = Some(1.5);
        let toujours = |genre| ChoixUtilisateur {
            portee: Portee::Global,
            genre,
            decision: Decision::Toujours,
        };
        let r = resoudre(
            &profil_vague_1(),
            &c,
            &[
                toujours(GenreTraitement::Equilibre),
                toujours(GenreTraitement::Loudness),
            ],
        );
        let (trace, p) = porte();
        let mut etages = etages_actifs(&r, OrigineDsd::Aucune, &p);
        let ids: Vec<&str> = etages.iter().map(|e| e.id()).collect();
        assert_eq!(
            ids,
            vec![
                "dsp-contextuel.desaccentuation",
                "dsp-contextuel.equilibre",
                "dsp-contextuel.loudness"
            ]
        );
        for e in &mut etages {
            e.preparer(format(44_100), 1_024).unwrap();
        }
        // Équilibre coupé : le canal fort retrouve 1,5 dB.
        etages[1].definir_bypass(true);
        assert!((trace.hausses()[0] - 1.5).abs() < 1e-9);
        // Loudness : coupé puis rallumé à volume bas ⇒ hausse ≤ 3 dB.
        etages[2].definir_volume_db(-40.0);
        etages[2].definir_bypass(true);
        assert_eq!(
            trace.hausses().len(),
            1,
            "couper le loudness est une baisse"
        );
        etages[2].definir_bypass(false);
        let h = trace.hausses();
        assert_eq!(h.len(), 2);
        assert!(h[1] > 2.5 && h[1] <= 3.0 + 1e-9, "{}", h[1]);
    }

    #[test]
    fn bypass_rend_l_entree_au_bit_pres_apres_le_fondu() {
        let r = resoudre(&profil_vague_1(), &cd_preaccentue(), &[]);
        let (_, p) = porte();
        let mut e = etages_actifs(&r, OrigineDsd::Aucune, &p).remove(0);
        e.preparer(format(48_000), 4_096).unwrap();
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
        let (_, p) = porte();
        let mut e = EtageContextuel::new(
            crate::traitements::Reglages::par_defaut(GenreTraitement::UltrasoniqueDsd)
                .construire(OrigineDsd::Dop)
                .unwrap(),
            p,
        );
        assert!(matches!(
            e.preparer(format(176_400), 512),
            Err(ErreurEtage::Reglages(_))
        ));
    }
}
