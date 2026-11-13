//! #8554 — le contour de loudness d'une zone, posé sur sa sortie locale.
//!
//! Le calcul vit dans `audio::contour_de_sonie` ; ici, seulement ce que la
//! zone demande (réglage, PURE, bit-perfect strict, volume fixe) et le
//! chemin qui le pousse vers la sortie locale qui joue. Les sorties réseau
//! ne l'appliquent pas encore : le chemin du signal le dit
//! (`output_not_supported`).

use super::*;
#[cfg(feature = "local-audio")]
use crate::audio::contour_de_sonie::{EtatDuContour, etat_de_la_zone};

impl PlaybackOrchestrator {
    /// Le contour que la zone demande à sa sortie locale, au format du flux
    /// qui va jouer. `None` hors état [`EtatDuContour::Actif`] : PURE et le
    /// bit-perfect strict ne reçoivent jamais de contour.
    #[cfg(feature = "local-audio")]
    pub(super) fn contour_de_sonie_voulu(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::outputs::local::ContourVoulu> {
        let (reglage, etat) = etat_de_la_zone(&self.db, zone_id, true);
        (etat == EtatDuContour::Actif).then_some(crate::outputs::local::ContourVoulu {
            reglage,
            sample_rate,
            channels,
        })
    }

    /// Réappliquer le contour de la zone à la sortie locale qui joue, sans
    /// attendre la piste suivante. `Immediate` quand une sortie locale l'a
    /// reçu ; sinon la prochaine lecture partira avec ce réglage.
    pub async fn refresh_zone_contour_de_sonie(&self, zone_id: i64) -> PorteeDuReglage {
        if self.pousser_le_contour_en_direct(zone_id).await {
            return PorteeDuReglage::Immediate;
        }
        match self.playback.get_state(zone_id).await.now_playing {
            None => PorteeDuReglage::RienNeJoue,
            Some(_) => PorteeDuReglage::PisteSuivante,
        }
    }

    async fn pousser_le_contour_en_direct(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            let Some((taux, canaux)) = local_output.current_format() else {
                return false;
            };
            let voulu = self.contour_de_sonie_voulu(zone_id, taux, canaux);
            let actif = voulu.is_some();
            local_output.set_contour_de_sonie(voulu);
            info!(zone_id, device_id = %device_id, actif, "zone_loudness_contour_refreshed_live");
            true
        }
    }
}
