//! Une enceinte Sendspin connectée devient une sortie, puis une zone (#3326, S2-c).
//!
//! Le protocole ne donne AUCUNE identité avant la poignée de main (pas
//! d'identifiant dans le TXT mDNS) : la sortie naît donc ici, à la connexion,
//! et non dans la découverte mDNS. Son identifiant est le `client_id` prouvé
//! par Noise ; seule une session appairée (PSK longue durée) arrive jusqu'ici.
//!
//! Les règles de zone sont celles des autres sorties réseau : une zone
//! supprimée (masquée) ne renaît pas, une zone connue repasse en ligne, une
//! nouvelle zone n'est créée que si la création automatique est permise.
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::event_bus::EventBus;
use tune_core::event_types::EventType;
use tune_core::outputs::OutputRegistry;
use tune_core::outputs::sendspin::{SendspinOutput, TYPE_DE_SORTIE, identifiant_de_sortie};
use tune_core::sendspin::lecteur::LiaisonLecteur;

/// Ce qu'il faut de l'état du serveur pour enregistrer une sortie et sa zone.
#[derive(Clone)]
pub struct RaccordZones {
    outputs: Arc<Mutex<OutputRegistry>>,
    db: Arc<dyn DbBackend>,
    bus: Arc<EventBus>,
}

impl RaccordZones {
    pub fn depuis_etat(state: &crate::state::AppState) -> Self {
        Self {
            outputs: state.outputs.clone(),
            db: state.backend.clone(),
            bus: state.event_bus.clone(),
        }
    }

    /// Enregistre la sortie et met sa zone en ligne. La garde rendue la
    /// retire quand la connexion se ferme.
    pub(super) async fn arrivee(
        &self,
        client_id: &str,
        nom: &str,
        liaison: LiaisonLecteur,
    ) -> GardeDepart {
        let id = identifiant_de_sortie(client_id);
        let sortie = {
            let mut reg = self.outputs.lock().await;
            reg.register(Box::new(SendspinOutput::new(
                nom.to_owned(),
                client_id,
                liaison,
            )));
            reg.get(&id)
        };
        self.bus.emit_typed(
            EventType::DeviceDiscovered,
            json!({"device_id": id, "name": nom, "device_type": TYPE_DE_SORTIE}),
        );
        let repo = ZoneRepo::with_backend(self.db.clone());
        if repo.is_device_hidden(&id) {
            info!(%id, "sendspin_zone_masquee_ignoree");
        } else if let Ok(Some(_)) = repo.get_by_device_id(&id) {
            crate::discovery_setup::set_zone_online(&self.bus, &self.db, &id, true);
            info!(%id, nom, "sendspin_zone_reconnectee");
        } else if !repo.zone_auto_create_autorise() {
            info!(%id, nom, "sendspin_zone_creation_auto_desactivee");
        } else {
            match repo.get_or_create(nom, Some(TYPE_DE_SORTIE), &id) {
                Ok((zid, true)) => {
                    self.bus.emit_typed(
                        EventType::ZoneCreated,
                        crate::discovery_setup::charge_utile_zone_creee(
                            &repo,
                            zid,
                            json!({"zone_id": zid, "name": nom, "device_id": id, "type": TYPE_DE_SORTIE}),
                        ),
                    );
                    info!(%id, nom, zone_id = zid, "sendspin_zone_creee");
                }
                Ok((_, false)) => {
                    crate::discovery_setup::set_zone_online(&self.bus, &self.db, &id, true);
                }
                Err(e) => warn!(%id, error = %e, "sendspin_zone_creation_echouee"),
            }
        }
        GardeDepart {
            zones: self.clone(),
            id,
            sortie,
        }
    }

    /// La connexion est fermée : la sortie disparaît, la zone passe hors ligne.
    ///
    /// Seulement si la sortie enregistrée est encore CELLE de cette connexion :
    /// une reconnexion rapide de la même enceinte a pu enregistrer la sienne
    /// sous le même identifiant entre-temps, et elle doit rester.
    async fn depart(&self, id: &str, sortie: Option<SortiePartagee>) {
        {
            let mut reg = self.outputs.lock().await;
            let meme = match (reg.get(id), sortie) {
                (Some(courante), Some(mienne)) => Arc::ptr_eq(&courante, &mienne),
                _ => false,
            };
            if !meme {
                return;
            }
            reg.remove(id);
        }
        crate::discovery_setup::set_zone_online(&self.bus, &self.db, id, false);
        self.bus
            .emit_typed(EventType::DeviceLost, json!({"device_id": id}));
        info!(%id, "sendspin_sortie_retiree");
    }
}

type SortiePartagee = Arc<Mutex<Box<dyn tune_core::outputs::OutputTarget>>>;

/// Retire la sortie quand le pilote s'arrête, par quelque chemin que ce soit.
pub(super) struct GardeDepart {
    zones: RaccordZones,
    id: String,
    sortie: Option<SortiePartagee>,
}

impl Drop for GardeDepart {
    fn drop(&mut self) {
        let zones = self.zones.clone();
        let id = std::mem::take(&mut self.id);
        let sortie = self.sortie.take();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { zones.depart(&id, sortie).await });
        }
    }
}
