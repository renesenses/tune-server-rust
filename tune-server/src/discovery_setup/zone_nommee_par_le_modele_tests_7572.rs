//! #7572 — une zone Sonos prend le nom du modèle, plus l'adresse IP.
//!
//! alec_eiffel (fil du 31/10/2026, rc9) : « la zone pourrait être nommée
//! automatiquement "Sonos Play:1" plutôt que son adresse IP, dans la mesure où
//! Tune identifie bien l'appareil ». Un Sonos s'annonce
//! « 192.168.1.19 - Sonos Play:1 Media Renderer » ; la découverte gardait la
//! partie avant « - », c'est-à-dire l'adresse.
//!
//! Vrai gestionnaire SSDP et vraie base ; annonce synthétique, aucun
//! abonnement UPnP (listener None).
use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

const HOTE: &str = "192.0.2.19";
const UDN: &str = "uuid:RINCON_000E58F1D2E401400_MR";

fn sonos(udn: &str, hote: &str) -> DiscoveredDevice {
    let mut dev = DiscoveredDevice::new(
        udn.into(),
        format!("{hote} - Sonos Play:1 Media Renderer"),
        OutputType::Dlna,
        hote.into(),
        1400,
    );
    dev.manufacturer = Some("Sonos, Inc.".into());
    dev.model = Some("Sonos Play:1".into());
    dev.capabilities.insert(
        "service_urls".into(),
        serde_json::json!({"avtransport": "/av", "renderingcontrol": "/rc"}),
    );
    dev
}

async fn annoncer(state: &crate::state::AppState, dev: &DiscoveredDevice) {
    let listener = None;
    let mut seen = std::collections::HashSet::new();
    handle_ssdp_discovered(
        dev,
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
}

fn nom_de_la_zone(state: &crate::state::AppState, udn: &str) -> String {
    ZoneRepo::with_backend(state.backend.clone())
        .get_by_device_id(udn)
        .unwrap()
        .expect("la zone existe")
        .name
}

#[test]
fn la_regle_du_nom_7572() {
    // Le Sonos : le modèle, plus l'adresse.
    assert_eq!(
        nom_de_zone_decouverte(
            "192.168.1.19 - Sonos Play:1 Media Renderer",
            "192.168.1.19",
            Some("Sonos Play:1")
        ),
        (
            "Sonos Play:1".to_string(),
            "Sonos Play:1 (192.168.1.19)".to_string()
        )
    );
    // Sans modèle : le reste du nom annoncé.
    assert_eq!(
        nom_de_zone_decouverte("192.168.1.19 - Sonos Play:1", "192.168.1.19", None).0,
        "Sonos Play:1"
    );
    // Rien d'autre à dire que l'adresse : l'adresse, faute de mieux.
    assert_eq!(
        nom_de_zone_decouverte("192.168.1.19", "192.168.1.19", None).0,
        "192.168.1.19"
    );
    // Témoins de non-régression : un nom de pièce reste le nom de la zone.
    assert_eq!(
        nom_de_zone_decouverte(
            "Chambre - Sonos Play:1",
            "192.168.1.19",
            Some("Sonos Play:1")
        ),
        ("Chambre".to_string(), "Chambre - Sonos Play:1".to_string())
    );
    assert_eq!(
        nom_de_zone_decouverte("Marantz ND8006", "192.168.1.30", Some("ND8006")),
        ("Marantz ND8006".to_string(), "Marantz ND8006".to_string())
    );
    // L'ancien nom n'est reconnu que s'il est une adresse.
    assert_eq!(
        ancien_nom_par_adresse("192.168.1.19 - Sonos Play:1", "192.168.1.19"),
        Some("192.168.1.19")
    );
    assert_eq!(
        ancien_nom_par_adresse("Chambre - Sonos Play:1", "192.168.1.19"),
        None
    );
}

#[tokio::test]
async fn un_sonos_decouvert_recoit_le_nom_de_son_modele_7572() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    annoncer(&state, &sonos(UDN, HOTE)).await;
    assert_eq!(
        nom_de_la_zone(&state, UDN),
        "Sonos Play:1",
        "#7572 : la zone Sonos porte l'adresse IP alors que le modèle est connu"
    );
}

#[tokio::test]
async fn deux_sonos_du_meme_modele_restent_distincts_7572() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    annoncer(&state, &sonos(UDN, HOTE)).await;
    annoncer(&state, &sonos("uuid:RINCON_SECOND_MR", "192.0.2.20")).await;
    assert_eq!(nom_de_la_zone(&state, UDN), "Sonos Play:1");
    assert_eq!(
        nom_de_la_zone(&state, "uuid:RINCON_SECOND_MR"),
        "Sonos Play:1 (192.0.2.20)"
    );
}

/// Une zone créée avant le correctif porte l'adresse : à la découverte
/// suivante, elle prend le nom du modèle.
#[tokio::test]
async fn une_zone_nommee_par_son_adresse_prend_le_nom_du_modele_7572() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    repo.create(HOTE, Some("dlna"), Some(UDN)).unwrap();

    annoncer(&state, &sonos(UDN, HOTE)).await;

    assert_eq!(nom_de_la_zone(&state, UDN), "Sonos Play:1");
}

/// 🔴 Une zone que l'utilisateur a nommée n'est JAMAIS renommée.
#[tokio::test]
async fn une_zone_nommee_par_l_utilisateur_garde_son_nom_7572() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    repo.create("Cuisine", Some("dlna"), Some(UDN)).unwrap();

    annoncer(&state, &sonos(UDN, HOTE)).await;

    assert_eq!(
        nom_de_la_zone(&state, UDN),
        "Cuisine",
        "#7572 : la découverte a écrasé le nom choisi par l'utilisateur"
    );
}

/// Le renommage est conditionnel EN BASE : si l'utilisateur renomme la zone
/// entre la lecture et l'écriture, son nom gagne.
#[tokio::test]
async fn le_renommage_ne_touche_pas_un_nom_change_entre_temps_7572() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some(UDN)).unwrap();
    assert_eq!(
        repo.renommer_si_nom_inchange(id, HOTE, "Sonos Play:1")
            .unwrap(),
        0
    );
    assert_eq!(repo.get(id).unwrap().unwrap().name, "Salon");
    repo.update_name(id, HOTE).unwrap();
    assert_eq!(
        repo.renommer_si_nom_inchange(id, HOTE, "Sonos Play:1")
            .unwrap(),
        1
    );
    assert_eq!(repo.get(id).unwrap().unwrap().name, "Sonos Play:1");
}
