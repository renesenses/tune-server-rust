//! #6912 — annonce AIR : observer séparément, adopter seulement sourdine et baisses.
use super::*;
use crate::db::zone_repo::Zone;
use crate::outputs::devialet_air::{DevialetAirOutput, TYPE_DE_SORTIE};

impl PositionPoller {
    pub(super) async fn relever_controles_air(&self, zone: &Zone, startup_at: &Instant) {
        let zone_id = zone.id.unwrap_or(0);
        let did = zone.output_device_id.as_deref();
        let observation = if zone.output_type.as_deref() == Some(TYPE_DE_SORTIE) {
            let arc = { self.outputs.lock().await.get(did.unwrap_or("")) };
            if let Some(arc) = arc {
                let Ok(sortie) = arc.try_lock() else {
                    return;
                };
                sortie
                    .as_any()
                    .downcast_ref::<DevialetAirOutput>()
                    .and_then(DevialetAirOutput::observation_volume)
            } else {
                None
            }
        } else {
            None
        };
        self.playback
            .noter_observation_volume_appareil(
                zone_id,
                zone.volume / 100.0,
                zone.muted,
                observation.clone(),
            )
            .await;
        let Some(observation) = observation else {
            return;
        };
        let Some(did) = did else {
            return;
        };
        // Le même verrou protège la consigne et sa grâce dans set_volume.
        // Aucun verrou de sortie n'est tenu ici : pas d'inversion avec son envoi.
        let file = self.orchestrator.volume_coalesceur.file(did, zone_id);
        let Ok(mut attente) = file.etat.try_lock() else {
            return;
        };
        let actuel = self.playback.get_state(zone_id).await;
        if attente.en_vol
            || startup_at.elapsed().as_secs() < 30
            || actuel
                .last_volume_set_at
                .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS)
        {
            return;
        }
        let repo = ZoneRepo::with_backend(self.db.clone());
        let Some(zone) = repo.get(zone_id).ok().flatten().filter(|z| {
            z.output_type.as_deref() == Some(TYPE_DE_SORTIE)
                && z.output_device_id.as_deref() == Some(did)
        }) else {
            return;
        };
        let volume = if zone.fixed_volume {
            actuel.volume
        } else {
            actuel
                .volume
                .min(zone.volume / 100.0)
                .min(observation.volume)
        };
        let en_base = if zone.fixed_volume {
            zone.volume
        } else {
            volume * 100.0
        };
        if (en_base - zone.volume).abs() <= 1e-9
            && actuel.muted == observation.muted
            && zone.muted == observation.muted
        {
            return;
        }
        if repo
            .adopter_controles_air_attendus(&zone, en_base, observation.muted)
            .unwrap_or(false)
            && self
                .playback
                .adopter_controles_air(
                    zone_id,
                    actuel.volume,
                    actuel.muted,
                    volume,
                    observation.muted,
                )
                .await
        {
            if let Some(ref mut confirme) = attente.confirme {
                *confirme = confirme.min(volume);
            }
            info!(
                zone_id,
                volume,
                muted = observation.muted,
                "etat_air_adopte_6912"
            );
        }
    }
}
