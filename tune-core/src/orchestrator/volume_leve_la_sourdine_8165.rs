//! #8165 — décision de Bertrand (12/11) : toucher au volume d'une zone en
//! sourdine LÈVE la sourdine, comme sur iOS ou Sonos.
//!
//! Avant, la règle dépendait de la sortie : Chromecast et AirPlay 2 levaient
//! la sourdine de l'appareil à chaque `set_volume` (sans que la zone le
//! sache : elle restait « en sourdine » à l'écran), alors que DLNA, la sortie
//! locale et les autres gardaient la zone muette. Désormais, toutes les
//! sorties suivent la même règle : chaque route qui porte un GESTE de volume
//! (`POST /playback/{id}/volume`, `PUT` et `PATCH /zones/{id}`, les pas
//! + / −, le volume de groupe, `SetVolume` du renderer UPnP) appelle
//! [`PlaybackOrchestrator::lever_la_sourdine_au_geste_de_volume`] une fois la
//! consigne appliquée. Pas l'orchestrateur lui-même : le sondeur et les
//! alarmes passent aussi par `set_volume_avec_garde`, et ce ne sont pas des
//! gestes.
//!
//! La consigne part d'abord (une sortie DLNA en sourdine la retient, #8151),
//! puis la sourdine est levée par [`PlaybackOrchestrator::set_mute`], qui
//! tient la base, l'état de lecture et la sortie d'accord.
//!
//! Hors geste (restauration au démarrage, relevé du sondeur, alarme, limite
//! de sécurité appliquée, volume fixe, PURE), rien ne change.

use super::*;

impl PlaybackOrchestrator {
    /// Lève la sourdine de la zone après un geste de volume. Rend `true`
    /// quand elle était en sourdine. Un refus de la sortie est journalisé et
    /// ne fait pas échouer le geste de volume, qui a déjà été appliqué.
    pub async fn lever_la_sourdine_au_geste_de_volume(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
    ) -> bool {
        if !self.zone_en_sourdine(zone_id).await {
            return false;
        }
        match self.set_mute(zone_id, false, device_id).await {
            Ok(()) => {
                info!(
                    zone_id,
                    device_id = device_id.unwrap_or(""),
                    "sourdine_levee_par_le_geste_de_volume"
                );
                true
            }
            Err(e) => {
                warn!(
                    zone_id,
                    device_id = device_id.unwrap_or(""),
                    error = %e,
                    "sourdine_non_levee_au_geste_de_volume"
                );
                false
            }
        }
    }
}
