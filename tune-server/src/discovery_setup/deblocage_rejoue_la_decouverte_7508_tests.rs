//! #7508 — débloquer un appareil recrée sa sortie, sans redémarrage.
//!
//! Le scénario du fil 2835 (B&W Zeppelin en DLNA), par le vrai chemin :
//! le scanner SSDP inscrit l'appareil pendant qu'il est ignoré, le vrai
//! consommateur d'évènements l'écarte, puis `DELETE /devices/{id}/ignore`
//! passe par la route HTTP. La sortie doit alors exister dans le registre que
//! la lecture interroge (`OutputRegistry::get`, celui qui rend
//! `output_not_found`).

use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;
use tune_core::db::ignored_device_repo::{IgnoredDevice, IgnoredDeviceRepo};
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

const ID: &str = "uuid:5f9ec1b3-zeppelin-7508";

fn zeppelin() -> DiscoveredDevice {
    let mut dev = DiscoveredDevice::new(
        ID.into(),
        "Zeppelin".into(),
        OutputType::Dlna,
        "192.168.1.50".into(),
        49152,
    );
    dev.capabilities.insert(
        "service_urls".into(),
        serde_json::json!({"avtransport": "/av", "renderingcontrol": "/rc"}),
    );
    dev
}

fn ignorer(state: &AppState) {
    IgnoredDeviceRepo::with_backend(state.backend.clone())
        .ignore(&IgnoredDevice {
            device_id: ID.into(),
            mac: String::new(),
            host: "192.168.1.50".into(),
            name: "Zeppelin".into(),
            device_type: "dlna".into(),
            created_at: None,
        })
        .unwrap();
}

/// Le consommateur de production, branché sur le scanner partagé comme le
/// fait `spawn_ssdp_handler` — sans lancer de multicast.
async fn brancher_le_scanner(state: &AppState) -> tokio::task::JoinHandle<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    state.scanner.set_event_tx(tx).await;
    spawn_ssdp_event_handler(state, &state.config, None, rx)
}

/// Attend que le consommateur ait vidé sa file : on ferme le canal du scanner
/// en le remplaçant, puis on attend la fin de la tâche.
async fn vider(state: &AppState, handler: tokio::task::JoinHandle<()>) {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    state.scanner.set_event_tx(tx).await;
    tokio::time::timeout(std::time::Duration::from_secs(20), handler)
        .await
        .expect("le consommateur SSDP doit finir")
        .unwrap();
}

async fn debloquer_par_http(state: &AppState) -> serde_json::Value {
    let response = crate::routes::devices::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/{ID}/ignore"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn debloquer_un_appareil_deja_connu_du_scanner_recree_sa_sortie() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    ignorer(&state);

    // 1. Découvert pendant qu'il est ignoré : écarté, aucune sortie.
    let handler = brancher_le_scanner(&state).await;
    state.scanner.inscrire_sans_reseau(zeppelin()).await;
    vider(&state, handler).await;
    assert!(
        state.outputs.lock().await.get(ID).is_none(),
        "un appareil ignoré ne doit pas être enregistré"
    );

    // 2. Ses réannonces ne réémettent rien : c'est le cœur du défaut.
    let handler = brancher_le_scanner(&state).await;
    state.scanner.inscrire_sans_reseau(zeppelin()).await;

    // 3. Débloqué par la route HTTP réelle.
    let corps = debloquer_par_http(&state).await;
    assert_eq!(corps["released"], serde_json::json!([ID]));
    vider(&state, handler).await;

    // 4. La sortie existe, là où la lecture la cherche — sans redémarrage.
    assert!(
        state.outputs.lock().await.get(ID).is_some(),
        "après le déblocage, la sortie DLNA doit exister : sans elle chaque \
         lecture échoue en « Device not yet discovered » jusqu'au redémarrage \
         (#7508, fil 2835)"
    );
    assert!(
        !crate::discovery_setup::appareil_ignore(&state.backend, &zeppelin()),
        "l'appareil ne doit plus être ignoré"
    );
}

/// Un appareil que le scanner n'a jamais vu n'est pas inventé : rien à
/// rejouer, et le déblocage reste une simple suppression.
#[tokio::test]
async fn debloquer_un_appareil_inconnu_du_scanner_ne_cree_rien() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    ignorer(&state);

    let corps = debloquer_par_http(&state).await;
    assert_eq!(corps["released"], serde_json::json!([ID]));
    assert!(state.outputs.lock().await.get(ID).is_none());
}

/// Une sortie déjà enregistrée n'est pas reconstruite (une lecture en cours
/// ne doit pas être coupée) : le rejeu ne vise que les absentes.
#[tokio::test]
async fn le_rejeu_ne_touche_pas_une_sortie_deja_enregistree() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let handler = brancher_le_scanner(&state).await;
    state.scanner.inscrire_sans_reseau(zeppelin()).await;
    vider(&state, handler).await;
    assert!(state.outputs.lock().await.get(ID).is_some());

    let rejoues = rejouer_les_appareils_debloques(&state, &[ID.to_string()]).await;
    assert_eq!(rejoues, 0);
}
