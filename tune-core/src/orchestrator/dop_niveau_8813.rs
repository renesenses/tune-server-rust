//! Protection du niveau demandé avant de choisir le transport DoP local.
use super::PlaybackOrchestrator;

impl PlaybackOrchestrator {
    pub(super) async fn attenuation_logicielle_demandee_pour_dop(
        &self,
        zone_id: i64,
        piste: crate::audio::replaygain::PisteRg<'_>,
    ) -> bool {
        if self.zone_audiophile(zone_id)
            || crate::audio::bitperfect_strict::zone_enabled(&self.db, zone_id)
            || !crate::audio::dop_ecarte_7265::protection_du_niveau_active(&self.db, zone_id)
        {
            return false;
        }
        let etat = self.playback.get_state(zone_id).await;
        let volume_fixe = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.fixed_volume);
        if etat.muted
            || (!volume_fixe
                && (etat.volume < 1.0 - 1e-6
                    || super::regles::gain_trim_db_enregistre(&self.db, zone_id) < -1e-6))
        {
            return true;
        }
        // La piste DEMANDÉE, y compris lors du préchargement : le gain de la
        // piste encore en lecture ne décrit pas celui du prochain DSD.
        if crate::audio::replaygain::facteur_de_zone(&self.db, zone_id, piste) < 1.0 - 1e-6 {
            return true;
        }
        // Une atténuation demandée suffit. Ne pas multiplier à nouveau la
        // compensation déjà portée par les échantillons de l'égaliseur.
        if self
            .load_eq_processor(zone_id, 44_100, 2)
            .is_some_and(|eq| {
                [0, 1]
                    .into_iter()
                    .any(|voie| eq.preamp_db(voie).is_some_and(|db| db < -1e-6))
            })
        {
            return true;
        }
        // L'égaliseur désactivé peut garder son préampli par sécurité. Ce
        // gain vit dans la sortie, et non dans le profil désormais éteint.
        #[cfg(feature = "local-audio")]
        if let Some(device_id) = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
        {
            let sortie = { self.outputs.lock().await.get(&device_id) };
            if let Some(sortie) = sortie {
                let garde = sortie.lock().await;
                if garde
                    .as_any()
                    .downcast_ref::<crate::outputs::local::LocalOutput>()
                    .and_then(|s| s.preampli_garde())
                    .is_some_and(|g| g.db < -1e-6)
                {
                    return true;
                }
            }
        }
        false
    }
}

impl PlaybackOrchestrator {
    /// Lire le constat de la sortie : le mode demandé ne prouve pas que le
    /// flux en cours est encore DoP (un repli PCM a pu avoir lieu).
    pub(super) async fn dop_actif_a_proteger(&self, zone_id: i64) -> bool {
        let zone = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let Some(zone) = zone.filter(|z| z.output_type.as_deref() == Some("local")) else {
            return false;
        };
        let Some(device_id) = zone.output_device_id else {
            return false;
        };
        let etat = self.playback.get_state(zone_id).await;
        let Some(np) = etat.now_playing else {
            return false;
        };
        if !self
            .attenuation_logicielle_demandee_pour_dop(zone_id, np.piste_rg())
            .await
        {
            return false;
        }
        let sortie = { self.outputs.lock().await.get(&device_id) };
        let Some(sortie) = sortie else {
            return false;
        };
        sortie
            .lock()
            .await
            .get_status()
            .await
            .is_ok_and(|s| s.dop_active)
    }

    /// Une commande de réglage ne réveille jamais une zone en pause. La
    /// reprise vérifiera à nouveau la protection avant le Resume matériel.
    pub async fn proteger_le_dop_en_lecture(&self, zone_id: i64) -> Result<bool, String> {
        let etat = self.playback.get_state(zone_id).await;
        if etat.state != crate::playback::PlayState::Playing
            || !self.dop_actif_a_proteger(zone_id).await
        {
            return Ok(false);
        }
        let position = super::dsp::position_a_reprendre(&etat);
        // Le flux DoP est arrêté avant création du PCM. Aucun gain n'est
        // appliqué aux marqueurs ; le nouveau flux part à la position gardée.
        let resultat =
            Box::pin(self.replay_zone_at_position(zone_id, position, "protection_niveau_dop"))
                .await;
        if let Err(ref erreur) = resultat {
            tracing::warn!(zone_id, error = %erreur, "protection_niveau_dop_relecture_refusee");
            if let Some(bus) = &self.event_bus {
                bus.emit(
                    "zone.playback_error",
                    serde_json::json!({
                        "zone_id": zone_id, "error": erreur, "fatal": true,
                        "code": "dsd_volume_protection_failed"
                    }),
                );
            }
        }
        resultat.map(|()| true)
    }
}
