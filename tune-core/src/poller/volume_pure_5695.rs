//! #5695 — sous PURE verrouillé, le sondeur n'adopte plus le volume du
//! renderer : il réimpose 100 %.
//!
//! Fil 2119 : zone « My Devialet » (DLNA), PURE actif avec « Forcer à 100 % »,
//! et pourtant le chemin du signal affichait `Volume 83%`. Ce chiffre est
//! `zones.volume`, la valeur en base. Or `Orchestrator::set_volume` ne
//! persiste jamais que la consigne de l'utilisateur ; la seule écriture qui
//! recopie en base ce que l'APPAREIL dit de lui-même est l'adoption du volume
//! par le sondeur (`poller/tick.rs`, trois sites). Elle ne regardait que
//! `fixed_volume`, jamais le verrou PURE : un volume changé depuis la
//! télécommande ou l'application du fabricant, ou un 0,83 parti d'un trim de
//! gain composé à tort, devenait le volume « officiel » de la zone.
//!
//! Le verrou promet 100 % : le sondeur le tient au lieu d'enregistrer
//! l'écart, et le DIT, une fois par zone — sur le canal non fatal de #2269
//! (`zone.playback_error`, `fatal: false`), puisque la lecture continue.

use super::*;

/// Le texte du bandeau. Le volume rapporté est nommé : c'est la seule trace,
/// côté auditeur, de ce que l'appareil a fait de lui-même.
pub(super) fn message_de_volume_reimpose(rapporte: f64, echec: Option<&str>) -> String {
    let pourcent = (rapporte.clamp(0.0, 1.0) * 100.0).round() as i32;
    match echec {
        None => format!(
            "Le volume de l'appareil a été changé hors de Tune ({pourcent} %), alors que \
             le mode PURE de cette zone impose 100 % : Tune l'a remis à 100 %."
        ),
        Some(cause) => format!(
            "Le volume de l'appareil a été changé hors de Tune ({pourcent} %), alors que \
             le mode PURE de cette zone impose 100 %, et Tune n'a pas pu le remettre à \
             100 % ({cause})."
        ),
    }
}

impl PositionPoller {
    /// Le renderer rapporte un volume que le sondeur s'apprête à adopter.
    ///
    /// Rend `true` quand la zone est en PURE verrouillé : l'adoption doit
    /// alors être SAUTÉE (rien n'est écrit en base), et le plein volume a été
    /// recommandé à l'appareil. Rend `false` hors verrou : rien n'est fait, le
    /// comportement historique s'applique.
    ///
    /// Appelée en DERNIER dans la condition d'adoption, donc seulement sur un
    /// vrai mouvement de l'appareil (`should_adopt_device_volume`) : un
    /// appareil qui refuse le 100 % et reste à 83 % n'est pas recommandé à
    /// chaque tick, faute de nouveau front.
    pub(super) async fn volume_pure_reimpose(&self, zone_id: i64, rapporte: f64) -> bool {
        if !(crate::audio::audiophile::volume_lock_enabled(&self.db, zone_id)
            && crate::audio::audiophile::zone_enabled(&self.db, zone_id))
        {
            return false;
        }
        let device_id = self.get_zone_device_id(zone_id);
        let echec = self
            .orchestrator
            .set_volume(zone_id, 1.0, device_id.as_deref())
            .await
            .err()
            .map(|e| e.to_string());
        let premiere_fois = self
            .volumes_pure_reimposes
            .lock()
            .map(|mut dits| dits.insert(zone_id))
            .unwrap_or(false);
        if !premiere_fois {
            debug!(zone_id, rapporte, ?echec, "pure_volume_reimpose");
            return true;
        }
        warn!(zone_id, rapporte, ?echec, "pure_volume_reimpose_annonce");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message_de_volume_reimpose(rapporte, echec.as_deref()),
                    // La lecture continue : c'est un avertissement (#2269).
                    "fatal": false,
                }),
            );
        }
        true
    }

    /// Rattrape, UNE fois par épisode de verrou, une zone PURE verrouillée
    /// dont le volume n'est pas à 100 % sans qu'aucun front ne l'ait signalé.
    ///
    /// [`Self::volume_pure_reimpose`] ne se déclenche que sur un mouvement de
    /// l'appareil. Or deux états arrivent sans mouvement : une base héritée
    /// où la zone est déjà à 83 % (trim composé à tort avant ce correctif,
    /// volume adopté par l'ancien sondeur) et le verrou GLOBAL armé sur une
    /// zone déjà en PURE, que la route de configuration ne commande pas. Le
    /// chemin du signal affichait alors `Volume 83%` indéfiniment.
    ///
    /// Une seule tentative par épisode : un appareil qui refuse le 100 % n'est
    /// pas recommandé à chaque tour. L'épisode se referme quand la zone est
    /// vue hors verrou.
    pub(super) async fn volume_pure_concilie(&self, zone_id: i64, volume_zone: f64, rapporte: f64) {
        let verrouille = crate::audio::audiophile::zone_enabled(&self.db, zone_id)
            && crate::audio::audiophile::volume_lock_enabled(&self.db, zone_id);
        let a_rattraper = {
            let Ok(mut faits) = self.volumes_pure_concilies.lock() else {
                return;
            };
            if verrouille {
                faits.insert(zone_id)
            } else {
                faits.remove(&zone_id);
                false
            }
        };
        let ecart = volume_zone < 0.999 || (rapporte > 0.001 && rapporte < 0.999);
        if !a_rattraper || !ecart {
            return;
        }
        let device_id = self.get_zone_device_id(zone_id);
        match self
            .orchestrator
            .set_volume(zone_id, 1.0, device_id.as_deref())
            .await
        {
            Ok(()) => info!(zone_id, volume_zone, rapporte, "pure_volume_concilie"),
            Err(e) => {
                warn!(zone_id, volume_zone, rapporte, error = %e, "pure_volume_concilie_echec")
            }
        }
    }
}
