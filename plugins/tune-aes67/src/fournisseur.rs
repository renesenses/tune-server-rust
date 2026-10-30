//! Le fournisseur de sorties (`register_output_provider`).
//!
//! Il ne découvre rien sur le réseau : il rend les flux que l'utilisateur a
//! déclarés. L'hôte l'interroge en boucle (`spawn_output_providers`) ; un
//! flux ajouté pendant que le serveur tourne apparaît donc au passage
//! suivant. Chaque flux n'est construit qu'UNE fois : construire une sortie
//! démarre son annonce SAP, et l'hôte jette les doublons — une sortie
//! construite pour rien enverrait une annonce puis un retrait.
//!
//! Sans Premium, la liste est vide et le journal le dit une fois.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use tracing::{info, warn};
use tune_core::outputs::traits::{OutputProvider, OutputTarget, ProviderContext};

use crate::config::FluxAes67;
use crate::droit::DroitPremium;
use crate::horloge::SourceHorloge;
use crate::sortie::{ReglagesEmission, SortieAes67};

pub const NOM_DU_FOURNISSEUR: &str = "aes67";

pub struct FournisseurAes67 {
    flux: Arc<RwLock<Vec<FluxAes67>>>,
    droit: Arc<dyn DroitPremium>,
    horloge: Arc<dyn SourceHorloge>,
    reglages: ReglagesEmission,
    deja_construits: Mutex<HashSet<String>>,
    refus_dit: AtomicBool,
}

impl FournisseurAes67 {
    pub fn new(
        flux: Arc<RwLock<Vec<FluxAes67>>>,
        droit: Arc<dyn DroitPremium>,
        horloge: Arc<dyn SourceHorloge>,
        reglages: ReglagesEmission,
    ) -> Self {
        Self {
            flux,
            droit,
            horloge,
            reglages,
            deja_construits: Mutex::new(HashSet::new()),
            refus_dit: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl OutputProvider for FournisseurAes67 {
    fn provider_name(&self) -> &str {
        NOM_DU_FOURNISSEUR
    }

    async fn discover(&self, _ctx: &ProviderContext) -> Vec<Box<dyn OutputTarget>> {
        if !self.droit.autorise().await {
            if !self.refus_dit.swap(true, Ordering::Relaxed) {
                info!("aes67_sans_premium: la Sortie AES67 demande Premium, aucun flux émis");
            }
            return Vec::new();
        }
        self.refus_dit.store(false, Ordering::Relaxed);
        let flux = self.flux.read().unwrap_or_else(|p| p.into_inner()).clone();
        let mut deja = self
            .deja_construits
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut sorties: Vec<Box<dyn OutputTarget>> = Vec::new();
        for f in flux {
            if deja.contains(&f.id) {
                continue;
            }
            if let Err(e) = f.valider() {
                warn!(id = %f.id, error = %e, "aes67_flux_invalide");
                continue;
            }
            match SortieAes67::new(
                f.clone(),
                self.horloge.clone(),
                self.droit.clone(),
                self.reglages.clone(),
            ) {
                Ok(s) => {
                    deja.insert(f.id.clone());
                    sorties.push(Box::new(s));
                }
                Err(e) => warn!(id = %f.id, error = %e, "aes67_sortie_non_construite"),
            }
        }
        sorties
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::droit::DroitDuBanc;
    use crate::horloge::HorlogeSysteme;
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::time::Duration;

    struct Refus;
    #[async_trait]
    impl DroitPremium for Refus {
        async fn autorise(&self) -> bool {
            false
        }
    }

    fn flux(id: &str, dernier_octet: u8) -> FluxAes67 {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "nom": id,
            "adresse": format!("239.69.1.{dernier_octet}"),
            "interface": "127.0.0.1",
        }))
        .unwrap()
    }

    /// SAP vers un port de la boucle locale : rien ne sort de la machine.
    fn reglages(ecoute: &UdpSocket) -> ReglagesEmission {
        let std::net::SocketAddr::V4(sap) = ecoute.local_addr().unwrap() else {
            panic!("IPv4 attendu");
        };
        ReglagesEmission {
            destination_sap: SocketAddrV4::new(Ipv4Addr::LOCALHOST, sap.port()),
            intervalle_sap: Duration::from_secs(3600),
            temps_reel: false,
            avance: Duration::ZERO,
            decalage_media: 0,
        }
    }

    #[tokio::test]
    async fn sans_premium_aucun_flux_n_est_rendu() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        let f = FournisseurAes67::new(
            Arc::new(RwLock::new(vec![flux("salon", 10)])),
            Arc::new(Refus),
            Arc::new(HorlogeSysteme::new()),
            reglages(&ecoute),
        );
        assert!(f.discover(&ProviderContext::default()).await.is_empty());
    }

    #[tokio::test]
    async fn chaque_flux_valide_est_rendu_une_seule_fois() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        let liste = Arc::new(RwLock::new(vec![flux("salon", 10)]));
        let f = FournisseurAes67::new(
            liste.clone(),
            Arc::new(DroitDuBanc),
            Arc::new(HorlogeSysteme::new()),
            reglages(&ecoute),
        );
        let premiers = f.discover(&ProviderContext::default()).await;
        assert_eq!(premiers.len(), 1);
        assert_eq!(premiers[0].device_id(), "aes67:salon");
        assert_eq!(premiers[0].output_type(), "aes67");
        assert!(f.discover(&ProviderContext::default()).await.is_empty());
        // Un flux ajouté en cours de route, et un flux invalide ignoré.
        {
            let mut l = liste.write().unwrap();
            l.push(flux("bureau", 11));
            let mut mauvais = flux("mauvais", 12);
            mauvais.adresse = Ipv4Addr::new(10, 0, 0, 1);
            l.push(mauvais);
        }
        let suivants = f.discover(&ProviderContext::default()).await;
        assert_eq!(suivants.len(), 1);
        assert_eq!(suivants[0].device_id(), "aes67:bureau");
    }
}
