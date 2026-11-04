//! #7084 — une zone AirPlay 2 servie sans son composant `airplay-daemon`.
//!
//! Sous Windows, l'antivirus avait retiré `airplay-daemon.exe` : Tune servait
//! la Devialet Mania par sa sortie AirPlay 1, sans rien en dire sur la zone.
//! La fiche de zone (liste et détail) porte désormais
//! `airplay2_component_missing`, avec le code et le message que rend aussi la
//! lecture. Le champ est ADDITIF et n'apparaît que sur une zone concernée.

use serde_json::{Value, json};
use tune_core::db::zone_repo::Zone;
use tune_core::outputs::airplay2::composant_manquant_7084::{
    CODE_AIRPLAY2_COMPONENT_MISSING, manque_pour, message_composant_manquant,
};

/// Pose `airplay2_component_missing` sur la charge utile d'une zone dont
/// l'appareil est servi sans son composant AirPlay 2.
pub(crate) fn injecter_composant_airplay2_manquant(
    obj: &mut serde_json::Map<String, Value>,
    zone: &Zone,
) {
    if zone.output_device_id.as_deref().is_some_and(manque_pour) {
        obj.insert(
            "airplay2_component_missing".into(),
            json!({
                "code": CODE_AIRPLAY2_COMPONENT_MISSING,
                "message": message_composant_manquant(),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use tune_core::outputs::airplay2::composant_manquant_7084::{retablir, signaler};

    const MANIA: &str = "airplay-mania-7084-zone";

    /// Le témoin : la zone d'une enceinte AirPlay 2 servie sans composant le
    /// dit, en liste comme en détail ; la même zone, composant revenu, ne dit
    /// plus rien.
    #[tokio::test]
    async fn la_zone_dit_le_composant_airplay2_manquant_7084() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone_id = ZoneRepo::with_backend(state.backend.clone())
            .create("Mania", Some("airplay2"), Some(MANIA))
            .unwrap();
        signaler(MANIA);

        let Json(liste) = list_zones(State(state.clone())).await;
        let zone = liste
            .as_array()
            .unwrap()
            .iter()
            .find(|z| z["id"].as_i64() == Some(zone_id))
            .cloned()
            .expect("la zone Mania doit être listée");
        let avertissement = &zone["airplay2_component_missing"];
        assert_eq!(
            avertissement["code"], CODE_AIRPLAY2_COMPONENT_MISSING,
            "la zone servie sans airplay-daemon doit le dire (#7084) : {zone}"
        );
        assert!(
            avertissement["message"]
                .as_str()
                .unwrap()
                .contains("AirPlay 2 component missing")
        );

        let detail = get_zone(State(state.clone()), Path(zone_id))
            .await
            .into_response();
        let corps = axum::body::to_bytes(detail.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(
            detail["airplay2_component_missing"]["code"], CODE_AIRPLAY2_COMPONENT_MISSING,
            "le détail de la zone doit le dire aussi : {detail}"
        );

        retablir(MANIA);
        let Json(liste) = list_zones(State(state)).await;
        let zone = liste
            .as_array()
            .unwrap()
            .iter()
            .find(|z| z["id"].as_i64() == Some(zone_id))
            .cloned()
            .unwrap();
        assert!(
            zone.get("airplay2_component_missing").is_none(),
            "composant revenu : plus d'avertissement ({zone})"
        );
    }
}
