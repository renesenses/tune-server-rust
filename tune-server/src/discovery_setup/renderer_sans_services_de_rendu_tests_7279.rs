//! #7279 — un renderer sans sortie de lecture enregistrée ne reçoit pas de
//! zone. Vrai gestionnaire SSDP et vraie base ; annonce synthétique, aucun
//! abonnement UPnP (listener None).
use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

const HOTE: &str = "192.0.2.79";
const UDN: &str = "uuid:innuos-zenith-7279";

fn innuos(services: serde_json::Value) -> DiscoveredDevice {
    let mut dev = DiscoveredDevice::new(
        UDN.into(),
        "Innuos Zenith".into(),
        OutputType::Dlna,
        HOTE.into(),
        49152,
    );
    dev.capabilities.insert("service_urls".into(), services);
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

#[tokio::test]
async fn un_renderer_sans_renderingcontrol_ne_recoit_pas_de_zone_7279() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    annoncer(&state, &innuos(serde_json::json!({"avtransport": "/av"}))).await;

    assert!(
        !state.outputs.lock().await.list().iter().any(|id| id == UDN),
        "sans RenderingControl, aucune sortie DLNA ne s'enregistre"
    );
    let repo = ZoneRepo::with_backend(state.backend.clone());
    assert!(
        repo.get_by_device_id(UDN).unwrap().is_none(),
        "#7279 : une zone a été créée pour un appareil sans sortie de lecture ; \
         il apparaît dans Zones sans figurer parmi les appareils de lecture"
    );
}

/// Une zone créée AVANT le correctif n'est pas remise « en ligne » par une
/// annonce qui ne donne toujours pas de sortie.
#[tokio::test]
async fn une_zone_existante_sans_sortie_n_est_pas_remise_en_ligne_7279() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Innuos Zenith", Some("dlna"), Some(UDN))
        .unwrap();
    repo.set_online_by_device(UDN, false).unwrap();

    annoncer(&state, &innuos(serde_json::json!({"avtransport": "/av"}))).await;

    let zone = repo.get(id).unwrap().expect("la zone reste en base");
    assert!(
        !zone.online,
        "#7279 : la zone d'un appareil sans sortie de lecture est remise en ligne"
    );
}

/// Témoin de non-régression : un renderer complet a toujours sa sortie et sa
/// zone.
#[tokio::test]
async fn un_renderer_complet_a_sa_sortie_et_sa_zone_7279() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    annoncer(
        &state,
        &innuos(serde_json::json!({"avtransport": "/av", "renderingcontrol": "/rc"})),
    )
    .await;

    assert!(state.outputs.lock().await.list().iter().any(|id| id == UDN));
    let repo = ZoneRepo::with_backend(state.backend.clone());
    assert!(repo.get_by_device_id(UDN).unwrap().is_some());
}
