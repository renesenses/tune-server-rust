//! #6299 — supprimer la zone d'un renderer DLNA découvert l'oublie aussi du
//! magasin des découverts. Un renderer parti du réseau n'apparaît plus dans
//! Réglages › Appareils : la suppression de sa zone est le seul geste qui
//! reste à l'utilisateur pour arrêter les re-sondages de chaque démarrage.

use super::*;
use crate::routes::devices::{
    DiscoveredDlnaDevice, persist_discovered_dlna, uuids_dlna_decouverts,
};

const PARTI: &str = "uuid:4d696e69-444c-164e-9d41-b827eb4c0a0b";
const RESTANT: &str = "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26";

#[tokio::test]
async fn supprimer_la_zone_d_un_dlna_decouvert_l_oublie_du_magasin() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let depot = ZoneRepo::with_backend(state.backend.clone());
    let mut zones = Vec::new();
    for (uuid, hote) in [(PARTI, "192.168.1.40"), (RESTANT, "192.168.1.11")] {
        persist_discovered_dlna(
            &state.backend,
            &DiscoveredDlnaDevice::new(
                uuid,
                &format!("http://{hote}:8019/root.xml"),
                "Renderer",
                hote,
                8019,
            ),
        );
        zones.push(depot.create("Renderer", Some("dlna"), Some(uuid)).unwrap());
    }

    let statut = delete_zone(State(state.clone()), Path(zones[0]))
        .await
        .into_response()
        .status();
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert_eq!(
        uuids_dlna_decouverts(&state.backend),
        vec![RESTANT.to_string()],
        "la zone du renderer parti est supprimée : il ne doit plus être re-sondé \
         au démarrage, et l'autre renderer reste"
    );
}
