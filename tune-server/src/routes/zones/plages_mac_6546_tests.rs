//! #6546 — la plage MAC reconnue et sa marque, publiées avec l'identité d'une
//! zone (`GET /zones`, `GET /zones/{id}`).

use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

fn etat() -> crate::state::AppState {
    crate::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appareil(mac: Option<&str>) -> DiscoveredDevice {
    let mut d = DiscoveredDevice::new(
        "uuid:plage-6546".into(),
        "Salon".into(),
        OutputType::Dlna,
        "192.168.1.30".into(),
        49152,
    );
    d.mac_address = mac.map(str::to_string);
    d
}

fn identite(
    state: &crate::state::AppState,
    zone_id: i64,
    detecte: Option<&DiscoveredDevice>,
) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    crate::routes::zones::inject_device_identity(&mut obj, &state.backend, zone_id, None, detecte);
    obj
}

/// Une MAC Sonos de la table : la plage et la marque sortent, jamais la MAC
/// complète.
#[test]
fn la_zone_publie_la_plage_et_la_marque_de_son_appareil() {
    let state = etat();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("uuid:plage-6546"))
        .unwrap();
    // 00:0E:58 est une plage Sonos de la table générée.
    let obj = identite(&state, zone_id, Some(&appareil(Some("00:0E:58:12:34:56"))));
    assert_eq!(obj["mac_prefix"], json!("00:0E:58"), "{obj:#?}");
    assert_eq!(obj["mac_vendor"], json!("Sonos"), "{obj:#?}");
    assert!(
        !obj.values()
            .any(|v| v.as_str() == Some("00:0E:58:12:34:56")),
        "la MAC complète ne doit pas sortir avec la zone : {obj:#?}"
    );
}

/// Plage hors table, puis pas de MAC, puis pas d'appareil : les clés sont
/// toujours là, à `null` quand il n'y a rien à dire.
#[test]
fn plage_inconnue_et_mac_absente_se_distinguent() {
    let state = etat();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("uuid:plage-6546"))
        .unwrap();

    let hors_table = identite(&state, zone_id, Some(&appareil(Some("02:00:00:AA:BB:CC"))));
    assert_eq!(hors_table["mac_prefix"], json!("02:00:00"));
    assert_eq!(hors_table["mac_vendor"], Value::Null);

    let sans_mac = identite(&state, zone_id, Some(&appareil(None)));
    assert_eq!(sans_mac["mac_prefix"], Value::Null);
    assert_eq!(sans_mac["mac_vendor"], Value::Null);
    assert!(sans_mac.contains_key("mac_prefix") && sans_mac.contains_key("mac_vendor"));

    let sans_appareil = identite(&state, zone_id, None);
    assert_eq!(sans_appareil["mac_prefix"], Value::Null);
    assert_eq!(sans_appareil["mac_vendor"], Value::Null);
}

/// #3660 — une identité détectée récusée ne revient pas par l'OUI.
#[test]
fn une_identite_recusee_ne_publie_pas_la_marque_par_l_oui() {
    let state = etat();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("uuid:plage-6546"))
        .unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(&crate::routes::zones::cle_identite_effacee(zone_id), "true")
        .unwrap();
    let obj = identite(&state, zone_id, Some(&appareil(Some("00:0E:58:12:34:56"))));
    assert_eq!(obj["identite_appareil_effacee"], json!(true));
    assert_eq!(obj["mac_vendor"], Value::Null, "{obj:#?}");
    assert_eq!(obj["mac_prefix"], Value::Null, "{obj:#?}");
}
