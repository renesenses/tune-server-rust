//! La source PCM `entree-audio`, EN DIRECT, inscrite auprès de
//! l'orchestrateur : une ligne de file `source = "entree-audio"`,
//! `source_id = <nom du périphérique>` s'ouvre ici.

use std::sync::Arc;

use tune_core::source_pcm::{Consommation, FluxDirect, FluxPcm, FournisseurPcm};

use crate::controleur::Controleur;

/// Le nom de la source, dans la file et dans `NowPlaying`.
pub const SOURCE: &str = "entree-audio";

pub struct FournisseurEntree {
    pub controleur: Arc<Controleur>,
}

impl FournisseurPcm for FournisseurEntree {
    fn ouvrir(&self, source_id: &str, _depuis_ms: u64) -> Result<FluxPcm, String> {
        Err(format!(
            "« {source_id} » est une entrée en direct : ni longueur ni avance possibles"
        ))
    }

    fn en_direct(&self) -> bool {
        true
    }

    fn ouvrir_direct(
        &self,
        source_id: &str,
        consommation: Consommation,
    ) -> Result<FluxDirect, String> {
        self.controleur.ouvrir_lecteur(source_id, consommation)
    }
}
