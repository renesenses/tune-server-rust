//! Le droit Premium de la Sortie AES67 (décision de Bertrand du 30/10/2026).
//!
//! Le juge est celui du serveur : `LicenseManager::check_feature` sur
//! [`Feature::Aes67`], comme Pont Roon et Concerts. Le trait n'existe que
//! pour que le banc d'essai émette sans base ni licence.

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::license::{Feature, LicenseManager};

#[async_trait]
pub trait DroitPremium: Send + Sync {
    async fn autorise(&self) -> bool;
}

/// La licence du serveur. `None` (hôte sans licence) vaut SANS Premium,
/// jamais l'inverse.
pub struct DroitDeLaLicence(pub Option<Arc<LicenseManager>>);

#[async_trait]
impl DroitPremium for DroitDeLaLicence {
    async fn autorise(&self) -> bool {
        match &self.0 {
            Some(l) => l.check_feature(Feature::Aes67).await,
            None => false,
        }
    }
}

/// Droit accordé sans condition : banc d'essai et tests seulement.
pub struct DroitDuBanc;

#[async_trait]
impl DroitPremium for DroitDuBanc {
    async fn autorise(&self) -> bool {
        true
    }
}
