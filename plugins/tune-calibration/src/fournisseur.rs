//! Le fournisseur PCM « calibration », FINI, inscrit auprès de
//! l'orchestrateur : une ligne de file `source = "calibration"`,
//! `source_id = <id de mesure>` s'ouvre ici et rend le [`Generateur`] du
//! déroulé de cette mesure.
//!
//! Aucune lecture sans action explicite : un déroulé n'existe qu'entre
//! `POST /zones/{zone}/mesures` et la fin de la mesure. En dehors, `ouvrir`
//! refuse et la zone ne joue rien.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tune_core::source_pcm::{FluxPcm, FournisseurPcm};

use crate::generateur::{Deroule, Generateur};

/// Le nom de la source, dans la file et dans `NowPlaying`.
pub const SOURCE: &str = "calibration";

#[derive(Default)]
pub struct FournisseurBalayage {
    deroules: Mutex<HashMap<String, Arc<Deroule>>>,
}

impl FournisseurBalayage {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Deroule>>> {
        self.deroules.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Arme le déroulé `id` : la zone pourra l'ouvrir.
    pub fn armer(&self, id: &str, deroule: Arc<Deroule>) {
        self.verrou().insert(id.to_string(), deroule);
    }

    /// Désarme : toute ouverture ultérieure est refusée.
    pub fn desarmer(&self, id: &str) {
        self.verrou().remove(id);
    }

    pub fn armes(&self) -> usize {
        self.verrou().len()
    }
}

impl FournisseurPcm for FournisseurBalayage {
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        let deroule =
            self.verrou().get(source_id).cloned().ok_or_else(|| {
                format!("aucune mesure de calibration armée sous « {source_id} »")
            })?;
        if depuis_ms != 0 {
            return Err("un balayage de mesure ne s'ouvre qu'à son début".into());
        }
        if deroule.annulee() {
            return Err("mesure annulée".into());
        }
        Ok(FluxPcm {
            format: deroule.plan.format,
            octets: deroule.plan.octets_max(),
            duree_ms: deroule.plan.duree_max_ms(),
            lecteur: Box::new(Generateur::new(deroule)),
        })
    }

    /// Jamais pré-armé : ouvrir le balayage, c'est commencer à le jouer.
    fn pre_armable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generateur::{Plan, Voies};

    #[test]
    fn sans_mesure_armee_rien_ne_s_ouvre() {
        let f = FournisseurBalayage::new();
        let e = f.ouvrir("m-1", 0).unwrap_err();
        assert!(e.contains("aucune mesure"), "{e}");
    }

    #[test]
    fn une_mesure_armee_s_ouvre_a_son_debut_seulement() {
        let f = FournisseurBalayage::new();
        let d = Deroule::new(Plan::defaut(48_000, 2.0, Voies::LesDeux));
        f.armer("m-1", d.clone());
        assert!(f.ouvrir("m-1", 1_000).is_err());
        let flux = f.ouvrir("m-1", 0).unwrap();
        assert_eq!(flux.octets, d.plan.octets_max());
        assert_eq!(flux.format.frequence, 48_000);
        assert!(d.ouvert());
        f.desarmer("m-1");
        assert!(f.ouvrir("m-1", 0).is_err());
    }
}
