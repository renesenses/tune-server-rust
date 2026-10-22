//! Le fournisseur de sorties Devialet AIR : une sortie par ampli entendu.
//!
//! Il passe par le même chemin que les fournisseurs hors arbre
//! (`spawn_output_providers`), qui l'interroge au démarrage puis toutes les
//! 60 s et ignore les identifiants déjà enregistrés. Option désactivée :
//! aucune écoute, aucune sortie.

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use super::ecoute::Ecoute;
use super::sortie::{ContexteAir, DevialetAirOutput};
use crate::outputs::traits::{OutputProvider, OutputTarget, ProviderContext};

/// Nom du fournisseur dans les journaux et le diagnostic.
pub const NOM_DU_FOURNISSEUR: &str = "devialet_air";

/// À l'ouverture de l'écoute, le temps d'entendre quelques annonces (une
/// toutes les 200 ms) avant de rendre la liste.
pub const ATTENTE_DES_PREMIERES_ANNONCES: Duration = Duration::from_millis(1_500);

pub struct FournisseurDevialetAir {
    ecoute: Arc<Ecoute>,
    contexte: ContexteAir,
    attente: Duration,
}

impl FournisseurDevialetAir {
    #[must_use]
    pub fn new(contexte: ContexteAir) -> Self {
        Self::avec_attente(contexte, ATTENTE_DES_PREMIERES_ANNONCES)
    }

    #[must_use]
    pub fn avec_attente(contexte: ContexteAir, attente: Duration) -> Self {
        Self {
            ecoute: Ecoute::nouvelle(),
            contexte,
            attente,
        }
    }

    /// L'écoute partagée par les sorties de ce fournisseur.
    #[must_use]
    pub fn ecoute(&self) -> Arc<Ecoute> {
        self.ecoute.clone()
    }
}

/// Le nom de zone d'un ampli : le nom qu'il annonce, sinon un nom générique.
#[must_use]
pub fn nom_de_zone(nom_annonce: &str) -> String {
    let nom = nom_annonce.trim();
    if nom.is_empty() {
        "Devialet Expert".to_string()
    } else {
        nom.to_string()
    }
}

#[async_trait::async_trait]
impl OutputProvider for FournisseurDevialetAir {
    fn provider_name(&self) -> &str {
        NOM_DU_FOURNISSEUR
    }

    async fn discover(&self, _ctx: &ProviderContext) -> Vec<Box<dyn OutputTarget>> {
        if !(self.contexte.activee)() {
            if self.ecoute.en_ecoute() {
                self.ecoute.arreter();
            }
            return Vec::new();
        }
        if !self.ecoute.en_ecoute() {
            if let Err(e) = self.ecoute.demarrer(self.contexte.ports.annonces) {
                warn!(erreur = %e, "devialet_air_ecoute_impossible");
                return Vec::new();
            }
            tokio::time::sleep(self.attente).await;
        }
        self.ecoute
            .presents()
            .into_iter()
            .map(|(mac, a)| {
                Box::new(DevialetAirOutput::new(
                    nom_de_zone(&a.annonce.nom),
                    mac,
                    self.ecoute.clone(),
                    self.contexte.clone(),
                )) as Box<dyn OutputTarget>
            })
            .collect()
    }
}
