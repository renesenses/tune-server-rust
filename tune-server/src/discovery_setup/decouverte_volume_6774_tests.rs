//! Témoins de rattachement mDNS : vraie base et vrai gestionnaire, sortie factice.
use super::*;
use tune_core::db::{settings_repo::SettingsRepo, zone_repo::ZoneRepo};
use tune_core::discovery::device::{DiscoveredDevice, OutputType};
use tune_core::outputs::mock::MockOutput;

const HOTE: &str = "192.0.2.50";
const MAC: &str = "AA:BB:CC:11:22:33";

async fn scenario(chemin: &str, meme_mac: bool, masquee: bool) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut dev = DiscoveredDevice::new(
        "cast-nouveau-6774".into(),
        "Salon 6774".into(),
        OutputType::Chromecast,
        HOTE.into(),
        8009,
    );
    let ancien = match chemin {
        "heritage" => tune_core::discovery::mdns::legacy_device_id(dev.device_type, HOTE, dev.port),
        "priorite" => format!("airplay-{HOTE}-7000"),
        _ => "cast-ancien-6774".into(),
    };
    let nom = if chemin == "mac" {
        "Zone renommee"
    } else {
        &dev.name
    };
    let ty = if chemin == "priorite" {
        "airplay"
    } else {
        "chromecast"
    };
    let id = repo.create(nom, Some(ty), Some(&ancien)).unwrap();
    repo.set_identity(id, "192.0.2.49", Some(MAC)).unwrap();
    dev.mac_address = Some(if meme_mac { MAC } else { "AA:BB:CC:44:55:66" }.into());
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set(&format!("zone_{id}_gain_trim_db"), "8.5").unwrap();
    s.set(&format!("zone_{id}_volume_limit_safety"), "0.5")
        .unwrap();
    if masquee {
        repo.delete(id).unwrap();
    }
    let sortie = MockOutput::new(&dev.id, &dev.name)
        .with_type("chromecast")
        .with_host(HOTE);
    enregistrer_la_sortie_mdns_et_sa_zone(
        &state.outputs,
        &state.backend,
        &state.event_bus,
        &state.playback,
        &dev,
        Box::new(sortie),
        "chromecast",
        false,
        AnnonceAppareil::Decouverte,
    )
    .await;
    let zone = repo
        .get_by_device_id(&dev.id)
        .unwrap()
        .expect("la zone doit être rattachée");
    assert_eq!(
        zone.id,
        Some(id),
        "{chemin} : pas de zone neuve qui masquerait le défaut #6774"
    );
    assert_eq!(
        s.get(&format!("zone_{id}_gain_trim_db"))
            .unwrap()
            .as_deref(),
        if meme_mac { Some("8.5") } else { None },
        "#6774 découverte {chemin} : trim positif transmis à un autre appareil, ou perdu sur la même MAC"
    );
    assert_eq!(
        s.get(&format!("zone_{id}_volume_limit_safety"))
            .unwrap()
            .as_deref(),
        Some("0.5"),
        "{chemin} : limite perdue"
    );
    if masquee {
        assert!(
            repo.is_device_hidden(&dev.id),
            "la zone masquée doit le rester"
        );
    }
}

#[tokio::test]
async fn meme_nom_autre_mac_ne_transmet_pas_le_trim_6774() {
    scenario("nom", false, false).await;
}
#[tokio::test]
async fn meme_nom_meme_mac_conserve_le_trim_6774() {
    scenario("nom", true, false).await;
}
#[tokio::test]
async fn mac_meme_appareil_renomme_conserve_le_trim_6774() {
    scenario("mac", true, false).await;
}
#[tokio::test]
async fn identifiant_herite_autre_appareil_ne_transmet_pas_le_trim_6774() {
    scenario("heritage", false, false).await;
}
#[tokio::test]
async fn montee_de_priorite_autre_appareil_ne_transmet_pas_le_trim_6774() {
    scenario("priorite", false, false).await;
}
#[tokio::test]
async fn nom_zone_masquee_autre_appareil_ne_transmet_pas_le_trim_6774() {
    scenario("nom", false, true).await;
}

// Annonce SSDP synthétique avec URLs de service inertes : on ne joue rien,
// aucun abonnement UPnP n'est créé (listener None).
async fn scenario_ssdp(par_mac: bool, mac_nouvelle: Option<&str>) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Renderer 6774", Some("dlna"), Some("uuid:ssdp-ancien-6774"))
        .unwrap();
    repo.set_identity(id, if par_mac { "192.0.2.49" } else { HOTE }, Some(MAC))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set(&format!("zone_{id}_gain_trim_db"), "8.5").unwrap();
    s.set(&format!("zone_{id}_volume_limit_safety"), "0.5")
        .unwrap();
    let mut dev = DiscoveredDevice::new(
        "uuid:ssdp-nouveau-6774".into(),
        "Renderer 6774".into(),
        OutputType::Dlna,
        HOTE.into(),
        49152,
    );
    dev.mac_address = mac_nouvelle.map(str::to_string);
    dev.capabilities.insert(
        "service_urls".into(),
        serde_json::json!({"avtransport":"/av","renderingcontrol":"/rc"}),
    );
    let listener = None;
    let mut seen = std::collections::HashSet::new();
    handle_ssdp_discovered(
        &dev,
        &state.outputs,
        &state.backend,
        &state.config,
        &state.event_bus,
        &listener,
        &state.playback,
        &state.license,
        &mut seen,
    )
    .await;
    let zone = repo
        .get_by_device_id(&dev.id)
        .unwrap()
        .expect("SSDP : la zone doit être rattachée");
    assert_eq!(
        zone.id,
        Some(id),
        "SSDP : pas de doublon qui masquerait le défaut"
    );
    assert_eq!(
        s.get(&format!("zone_{id}_gain_trim_db"))
            .unwrap()
            .as_deref(),
        if mac_nouvelle == Some(MAC) {
            Some("8.5")
        } else {
            None
        },
        "#6774 découverte SSDP : trim transmis sur adresse seule, ou perdu sur la même MAC"
    );
    assert_eq!(
        s.get(&format!("zone_{id}_volume_limit_safety"))
            .unwrap()
            .as_deref(),
        Some("0.5")
    );
}
#[tokio::test]
async fn ssdp_meme_adresse_autre_mac_ne_transmet_pas_le_trim_6774() {
    scenario_ssdp(false, Some("AA:BB:CC:44:55:66")).await;
}
#[tokio::test]
async fn ssdp_meme_adresse_sans_mac_ne_transmet_pas_le_trim_6774() {
    scenario_ssdp(false, None).await;
}
#[tokio::test]
async fn ssdp_meme_adresse_meme_mac_conserve_le_trim_6774() {
    scenario_ssdp(false, Some(MAC)).await;
}
#[tokio::test]
async fn ssdp_nouvelle_adresse_meme_mac_conserve_le_trim_6774() {
    scenario_ssdp(true, Some(MAC)).await;
}
