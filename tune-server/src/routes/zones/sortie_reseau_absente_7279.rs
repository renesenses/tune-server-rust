//! #7279 — une zone réseau dont la sortie de lecture n'est pas enregistrée
//! dit pourquoi.
//!
//! Innuos Zenith, fil 2495 : l'appareil apparaissait dans Zones, « en
//! ligne » puisque la découverte le voyait, mais aucune sortie de lecture ne
//! lui correspondait (il lui manquait un service de rendu UPnP). Le client web
//! retire de la liste des appareils de lecture tout appareil déjà rattaché à
//! une zone : il n'était donc nulle part, et rien ne disait pourquoi.
//!
//! La découverte ne crée plus de zone dans ce cas ; ce module couvre les zones
//! créées avant, et toute zone réseau dont la sortie manque.

use serde_json::json;
use tune_core::db::zone_repo::Zone;
use tune_core::discovery::device::DiscoveredDevice;

use crate::discovery_setup::SERVICES_DE_RENDU;

/// Pose `sortie_reseau_absente` sur une zone DLNA ou OpenHome et, quand la
/// découverte voit l'appareil sans que sa sortie soit enregistrée,
/// `raison_sortie_absente` et `services_manquants`. Rien n'est posé pour les
/// autres types : la règle des zones locales est
/// [`super::rattacher_asio_6120::injecter_sortie_absente`].
///
/// Pas `sortie_absente` : le client web propose « Rattacher » (à une sortie
/// ASIO, #6120) sur toute zone qui le porte à `true`, geste que le serveur
/// refuse pour une zone réseau.
pub(crate) fn injecter_raison_sortie_reseau_absente(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    zone: &Zone,
    appareils_vus: &[DiscoveredDevice],
    sorties_enregistrees: &std::collections::HashSet<String>,
) {
    let type_de_sortie = zone.output_type.as_deref();
    if !matches!(type_de_sortie, Some("dlna" | "openhome")) {
        return;
    }
    let Some(device_id) = zone.output_device_id.as_deref() else {
        return;
    };
    let absente = !sorties_enregistrees.contains(device_id);
    obj.insert("sortie_reseau_absente".into(), json!(absente));
    if !absente {
        return;
    }
    // Un appareil que la découverte ne voit pas est simplement hors ligne :
    // `online` le dit déjà.
    let Some(appareil) = appareils_vus.iter().find(|d| d.id == device_id) else {
        return;
    };
    let manquants = services_de_rendu_manquants(appareil, type_de_sortie);
    let raison = if manquants.is_empty() {
        "sortie_non_enregistree"
    } else {
        "services_de_rendu_manquants"
    };
    obj.insert("raison_sortie_absente".into(), json!(raison));
    obj.insert("services_manquants".into(), json!(manquants));
}

fn services_de_rendu_manquants(
    appareil: &DiscoveredDevice,
    type_de_sortie: Option<&str>,
) -> Vec<&'static str> {
    if type_de_sortie != Some("dlna") {
        return Vec::new();
    }
    let annonces = appareil
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();
    SERVICES_DE_RENDU
        .into_iter()
        .filter(|cle| !annonces.contains_key(*cle))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::discovery::device::OutputType;

    const UDN: &str = "uuid:innuos-zenith-7279";

    fn zone(output_type: &str) -> Zone {
        Zone {
            id: Some(7),
            name: "Innuos Zenith".into(),
            output_type: Some(output_type.into()),
            output_device_id: Some(UDN.into()),
            volume: 30.0,
            muted: false,
            online: true,
            gapless_enabled: true,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume: false,
            autoplay_enabled: false,
        }
    }

    fn appareil(services: serde_json::Value) -> DiscoveredDevice {
        let mut dev = DiscoveredDevice::new(
            UDN.into(),
            "Innuos Zenith".into(),
            OutputType::Dlna,
            "192.0.2.79".into(),
            49152,
        );
        dev.capabilities.insert("service_urls".into(), services);
        dev
    }

    fn injecter(
        z: &Zone,
        vus: &[DiscoveredDevice],
        enregistrees: &[&str],
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut obj = serde_json::Map::new();
        let enregistrees = enregistrees.iter().map(|s| s.to_string()).collect();
        injecter_raison_sortie_reseau_absente(&mut obj, z, vus, &enregistrees);
        obj
    }

    #[test]
    fn une_zone_vue_sans_sortie_dit_le_service_qui_manque_7279() {
        let vus = [appareil(json!({"avtransport": "/av"}))];
        let obj = injecter(&zone("dlna"), &vus, &[]);
        assert_eq!(
            obj.get("sortie_reseau_absente"),
            Some(&json!(true)),
            "#7279 : la zone de l'Innuos est publiée sans dire que sa sortie de lecture manque"
        );
        assert_eq!(
            obj.get("raison_sortie_absente"),
            Some(&json!("services_de_rendu_manquants")),
            "#7279 : aucune raison publiée pour une zone sans sortie de lecture"
        );
        assert_eq!(
            obj.get("services_manquants"),
            Some(&json!(["renderingcontrol"]))
        );
    }

    #[test]
    fn une_zone_dont_la_sortie_est_enregistree_ne_porte_aucune_raison_7279() {
        let vus = [appareil(
            json!({"avtransport": "/av", "renderingcontrol": "/rc"}),
        )];
        let obj = injecter(&zone("dlna"), &vus, &[UDN]);
        assert_eq!(obj.get("sortie_reseau_absente"), Some(&json!(false)));
        assert!(obj.get("raison_sortie_absente").is_none());
    }

    #[test]
    fn un_appareil_hors_ligne_n_a_pas_de_raison_inventee_7279() {
        let obj = injecter(&zone("dlna"), &[], &[]);
        assert_eq!(obj.get("sortie_reseau_absente"), Some(&json!(true)));
        assert!(
            obj.get("raison_sortie_absente").is_none(),
            "un appareil que la découverte ne voit pas est hors ligne, rien de plus"
        );
    }

    #[test]
    fn une_zone_reseau_ne_propose_pas_le_rattachement_asio_7279() {
        for ty in ["dlna", "openhome"] {
            let obj = injecter(&zone(ty), &[], &[]);
            assert!(
                obj.get("sortie_absente").is_none(),
                "#7279 : la zone {ty} porte `sortie_absente` ; le client web y \
                 propose « Rattacher » à une sortie ASIO, que le serveur refuse \
                 pour une zone réseau"
            );
        }
    }

    #[test]
    fn les_zones_locales_et_autres_types_ne_sont_pas_touchees_7279() {
        for ty in ["local", "airplay", "browser"] {
            let obj = injecter(&zone(ty), &[], &[]);
            assert!(obj.is_empty(), "{ty} : rien ne doit être posé");
        }
    }
}
