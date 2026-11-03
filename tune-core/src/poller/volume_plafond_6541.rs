//! #6541 — un renderer qui remonte un volume au-dessus de la limite de
//! sécurité de sa zone n'est pas adopté : le sondeur le ramène sous la
//! limite.
//!
//! La limite de sécurité (`audio::volume_limites`) borne toutes les consignes
//! de Tune. Reste ce que l'appareil fait de lui-même : télécommande, molette,
//! application du fabricant, ou un renderer qui se réveille à 100 %. Les trois
//! sites d'adoption de `tick.rs` recopiaient ce volume en base ; au-dessus de
//! la limite, ils le refusent désormais, et Tune recommande à l'appareil la
//! consigne de la zone (déjà sous la limite), bornée une fois encore.
//!
//! La recommande part sur FRONT : une fois par valeur rapportée (à
//! [`TOLERANCE_RENDERER`] près), pour ne pas marteler à chaque tour un
//! appareil qui refuse la commande. L'épisode se referme quand l'appareil
//! repasse sous la limite.
//!
//! Le cas « renderer à 100 % » de #5662 (Tune signale et n'impose rien,
//! décision du 07/10) ne s'applique pas ici : la limite est un réglage que
//! l'auditeur a posé lui-même sur cette zone, précisément pour que rien ne la
//! dépasse. Sans limite, rien ne change. En PURE ou à volume fixe, la limite
//! est ignorée (`lire_effectives`) : PURE verrouillé garde son 100 %.
use super::*;
use crate::audio::volume_limites::{self, TOLERANCE_RENDERER};

impl PositionPoller {
    /// Rend `true` quand `rapporte` dépasse la limite de sécurité de la zone :
    /// l'adoption doit alors être sautée. La consigne est recommandée à
    /// l'appareil au premier tour de chaque épisode (ou quand la valeur
    /// rapportée bouge).
    pub(super) async fn volume_plafond_reimpose(&self, zone_id: i64, rapporte: f64) -> bool {
        // En PURE ou à volume fixe, la limite n'a pas cours (décision du
        // 21/10) : ni recommande, ni adoption sautée.
        let limites = volume_limites::lire_effectives(&self.db, zone_id);
        if !limites.depasse(rapporte) {
            if let Ok(mut vus) = self.volumes_plafonnes.lock() {
                vus.remove(&zone_id);
            }
            return false;
        }
        let nouveau_front = self
            .volumes_plafonnes
            .lock()
            .map(|mut vus| {
                let deja = vus
                    .get(&zone_id)
                    .is_some_and(|v| (v - rapporte).abs() <= TOLERANCE_RENDERER);
                vus.insert(zone_id, rapporte);
                !deja
            })
            .unwrap_or(false);
        if !nouveau_front {
            return true;
        }
        let consigne = self.playback.get_state(zone_id).await.volume;
        let cible = limites.borner_appareil(consigne);
        let device_id = self.get_zone_device_id(zone_id);
        match self
            .orchestrator
            .set_volume(zone_id, cible, device_id.as_deref())
            .await
        {
            Ok(()) => warn!(zone_id, rapporte, cible, "volume_plafond_reimpose"),
            Err(e) => warn!(zone_id, rapporte, cible, error = %e, "volume_plafond_reimpose_echec"),
        }
        true
    }
}
