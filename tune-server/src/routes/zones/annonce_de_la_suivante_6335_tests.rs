//! #6335 — DSF sans enchaînement sur une zone DLNA dont le renderer annonce
//! le DSD et `SetNextAVTransportURI` (fil 2567 : le FLAC enchaîne, le DSF non).
//!
//! Le correctif de #6910 n'arme un suivant DSD sur DLNA que si le SCPD
//! d'AVTransport annonce `SetNextAVTransportURI`. Or seuls la découverte SSDP
//! et la relance du démarrage donnaient à la sortie l'URL de ce SCPD. L'ajout
//! manuel, le nouveau sondage d'un renderer muet en multidiffusion (#1126), le
//! balayage manuel et la création de zone construisaient une sortie sans elle :
//! verdict `Inconnue` jusqu'à la prochaine découverte SSDP, donc
//! `gapless_skipped_dsd_next_dlna` à chaque piste DSD, alors que le FLAC
//! s'arme même sur `Inconnue`.
//!
//! Ce banc construit la sortie par chacun de ces chemins contre un faux
//! renderer HTTP (descriptif + SCPD, rien n'est joué) et demande à la VRAIE
//! `DlnaOutput` ce qu'elle sait de `SetNextAVTransportURI`.
//!
//! Le balayage manuel (`scan_devices`) passe par le scanner SSDP : il n'est
//! pas rejoué ici, il reçoit la même ligne que les autres chemins.

use crate::routes::devices::{DiscoveredDlnaDevice, ManualDevice};
use crate::state::AppState;
use axum::{Router, routing::get};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tune_core::outputs::traits::AnnonceSuivante;

const UDN: &str = "uuid:6335-faux-renderer-dsd";

const SCPD_AVEC_SETNEXT: &str = "<?xml version=\"1.0\"?>\
<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><actionList>\
<action><name>SetAVTransportURI</name></action>\
<action><name>SetNextAVTransportURI</name></action>\
<action><name>Play</name></action>\
</actionList></scpd>";

fn descriptif() -> String {
    format!(
        "<?xml version=\"1.0\"?>\
<root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion>\
<device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>\
<friendlyName>Renderer DSD factice</friendlyName><manufacturer>Banc</manufacturer>\
<modelName>Banc 6335</modelName><UDN>{UDN}</UDN><serviceList>\
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>\
<serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>\
<SCPDURL>/avt/scpd.xml</SCPDURL><controlURL>/avt/control</controlURL>\
<eventSubURL>/avt/event</eventSubURL></service>\
<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>\
<serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>\
<SCPDURL>/rc/scpd.xml</SCPDURL><controlURL>/rc/control</controlURL>\
<eventSubURL>/rc/event</eventSubURL></service>\
<service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>\
<serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
<SCPDURL>/cm/scpd.xml</SCPDURL><controlURL>/cm/control</controlURL>\
<eventSubURL>/cm/event</eventSubURL></service>\
</serviceList></device></root>"
    )
}

/// Faux renderer : sert son descriptif et le SCPD d'AVTransport, compte les
/// lectures du SCPD. Rend son port.
async fn faux_renderer() -> (u16, Arc<AtomicUsize>) {
    let lectures = Arc::new(AtomicUsize::new(0));
    let compte = lectures.clone();
    let app = Router::new()
        .route("/description.xml", get(|| async { descriptif() }))
        .route(
            "/avt/scpd.xml",
            get(move || {
                let compte = compte.clone();
                async move {
                    compte.fetch_add(1, Ordering::Relaxed);
                    SCPD_AVEC_SETNEXT
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    (port, lectures)
}

/// Ce que la sortie enregistrée sous `device_id` dit de `SetNextAVTransportURI`.
async fn annonce_de(state: &AppState, device_id: &str) -> AnnonceSuivante {
    let sortie = state
        .outputs
        .lock()
        .await
        .get(device_id)
        .unwrap_or_else(|| panic!("aucune sortie au registre pour {device_id}"));
    sortie.lock().await.annonce_la_suivante().await
}

fn verifier(chemin: &str, annonce: AnnonceSuivante, lectures: &AtomicUsize) {
    assert_eq!(
        annonce,
        AnnonceSuivante::Annoncee,
        "#6335 : la sortie DLNA posée par {chemin} ne connaît pas le SCPD d'AVTransport \
         (annonce {annonce:?}) — aucun suivant DSD ne sera armé \
         (gapless_skipped_dsd_next_dlna)"
    );
    assert_eq!(
        lectures.load(Ordering::Relaxed),
        1,
        "{chemin} : le SCPD d'AVTransport doit être lu sur le renderer"
    );
}

/// Ajout manuel d'un appareil par son adresse (`manual_dlna_device_registered`).
#[tokio::test]
async fn ajout_manuel_connait_l_annonce_de_la_suivante() {
    let (port, lectures) = faux_renderer().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let (device_id, _, _) = crate::routes::devices::register_manual_device(
        &state,
        &ManualDevice {
            r#type: "dlna".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            name: Some("Renderer DSD factice".to_string()),
        },
    )
    .await
    .expect("l'ajout manuel doit enregistrer le renderer");
    verifier(
        "l'ajout manuel",
        annonce_de(&state, &device_id).await,
        &lectures,
    );
}

/// Nouveau sondage d'un renderer muet en multidiffusion (#1126).
#[tokio::test]
async fn nouveau_sondage_connait_l_annonce_de_la_suivante() {
    let (port, lectures) = faux_renderer().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let dev = DiscoveredDlnaDevice::new(
        UDN,
        &format!("http://127.0.0.1:{port}/description.xml"),
        "Renderer DSD factice",
        "127.0.0.1",
        port,
    );
    crate::routes::devices::register_discovered_dlna(&state, &dev)
        .await
        .unwrap_or_else(|e| panic!("le nouveau sondage doit enregistrer le renderer : {e:?}"));
    verifier(
        "le nouveau sondage",
        annonce_de(&state, UDN).await,
        &lectures,
    );
}

/// Création de zone, appareil du cache sans URLs de service : le descriptif
/// est relu (`dlna_output_registered_via_description`).
#[tokio::test]
async fn creation_de_zone_par_le_descriptif_connait_l_annonce_de_la_suivante() {
    let (port, lectures) = faux_renderer().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let mut dev = tune_core::discovery::device::DiscoveredDevice::new(
        UDN.to_string(),
        "Renderer DSD factice".to_string(),
        tune_core::discovery::device::OutputType::Dlna,
        "127.0.0.1".to_string(),
        port,
    );
    dev.location = Some(format!("http://127.0.0.1:{port}/description.xml"));
    assert!(super::register_dlna_output_from_device(&dev, &state).await);
    verifier(
        "la création de zone (descriptif)",
        annonce_de(&state, UDN).await,
        &lectures,
    );
}

/// Création de zone, appareil du cache SSDP avec ses URLs de service
/// (`dlna_output_registered_on_zone_create`).
#[tokio::test]
async fn creation_de_zone_depuis_le_cache_connait_l_annonce_de_la_suivante() {
    let (port, lectures) = faux_renderer().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let mut dev = tune_core::discovery::device::DiscoveredDevice::new(
        UDN.to_string(),
        "Renderer DSD factice".to_string(),
        tune_core::discovery::device::OutputType::Dlna,
        "127.0.0.1".to_string(),
        port,
    );
    // Ce que `ssdp::build_renderer_device` dépose dans les capacités.
    dev.capabilities.insert(
        "service_urls".to_string(),
        serde_json::json!({ "avtransport": "/avt/control", "renderingcontrol": "/rc/control" }),
    );
    dev.capabilities.insert(
        "scpd_urls".to_string(),
        serde_json::json!({ "avtransport": "/avt/scpd.xml", "renderingcontrol": "/rc/scpd.xml" }),
    );
    assert!(super::register_dlna_output_from_device(&dev, &state).await);
    verifier(
        "la création de zone (cache)",
        annonce_de(&state, UDN).await,
        &lectures,
    );
}
