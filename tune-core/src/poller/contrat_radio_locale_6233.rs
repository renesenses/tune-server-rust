//! #6233 — une radio sur une sortie LOCALE : le contrat de signal (ouverture
//! exclusive, bit-perfect, PURE) est relu à chaque tour, pas toutes les 15 s.
//!
//! Jean Valjean (Windows 11, WASAPI, fil 1825) : « de quelques secondes à une
//! à deux dizaines de secondes entre le clic et l'affichage correct » de PURE
//! et Bit-perfect sur une radio. Une zone radio n'est sondée que toutes les
//! `RADIO_POLL_INTERVAL_SECS` (15 s), quel que soit le type de sortie : ce
//! rythme protège les renderers DLNA (DMP-A8) de quatre appels SOAP par
//! seconde sur un flux infini. Or c'est ce sondage qui relève le contrat que
//! la sortie publie à l'ouverture du périphérique et l'annonce au client
//! (#4559). Le contrat attendait donc le prochain sondage radio.
//!
//! Une sortie locale n'a pas de SOAP : la relire coûte trois atomiques et un
//! verrou en mémoire. Entre deux sondages radio, ce module relit SEULEMENT
//! son contrat et l'annonce s'il a changé. Le reste du sondage radio
//! (transport, volume, métadonnées de la station, qui interrogent une API
//! externe) garde son rythme de 15 s.
use super::*;

/// La sortie se relit-elle entre deux sondages radio ? Seules les sorties
/// locales, en mémoire : un renderer réseau garde le rythme de 15 s.
pub(super) fn contrat_relu_entre_deux_sondages_radio(device_id: &str) -> bool {
    device_id.starts_with("local:")
}

impl PositionPoller {
    /// Relit le contrat de signal d'une sortie locale qui joue une radio, entre
    /// deux sondages radio, et l'annonce au client s'il a changé — même
    /// référence (`ZonePollState::contrat_de_signal_a_change`) que le sondage
    /// complet, donc une annonce et une seule par changement.
    pub(super) async fn relire_le_contrat_local_entre_deux_sondages_radio(
        &self,
        zone_id: i64,
        device_id: &str,
        ps: &mut ZonePollState,
    ) {
        let Some(output_arc) = ({ self.outputs.lock().await.get(device_id) }) else {
            return;
        };
        let Ok((_, signal_path, dsp_metrics, _, transformations, _)) =
            get_status_with_signal_path_bounded(&output_arc, *STATUS_POLL_TIMEOUT).await
        else {
            // Une sonde en échec est l'affaire du sondage complet, qui compte
            // les erreurs et recule : rien à dire ici.
            return;
        };
        let a_change =
            ps.contrat_de_signal_a_change(signal_path.as_ref(), transformations.as_ref());
        if !a_change {
            return;
        }
        self.playback
            .set_output_signal_path(zone_id, signal_path)
            .await;
        self.playback
            .set_output_dsp_metrics(zone_id, dsp_metrics)
            .await;
        self.playback
            .set_transformations_reelles(zone_id, transformations)
            .await;
        if let Some(ref bus) = self.event_bus {
            info!(
                zone_id,
                device = %device_id,
                "contrat_de_signal_radio_locale_annonce"
            );
            bus.emit("zone.updated", serde_json::json!({ "zone_id": zone_id }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seules_les_sorties_locales_sont_relues_entre_deux_sondages_radio() {
        assert!(contrat_relu_entre_deux_sondages_radio(
            "local:Haut-parleurs"
        ));
        assert!(!contrat_relu_entre_deux_sondages_radio("dlna:uuid:1234"));
        assert!(!contrat_relu_entre_deux_sondages_radio(
            "bluos-192.168.1.23-11000"
        ));
    }
}
