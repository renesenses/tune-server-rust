//! #6961 — un renderer DLNA allumé AVANT le serveur doit être retrouvé, et sa
//! zone remise en ligne, sans qu'on redémarre l'appareil.
//!
//! Le cas de Silviu (NanoPlayer, rc7) : l'appareil a redémarré pendant que le
//! serveur était arrêté. Son `NOTIFY` de démarrage est parti quand personne
//! n'écoutait, et le port de sa description n'est plus celui du magasin des
//! renderers connus. La sonde HTTP de démarrage échouait donc, rien d'autre ne
//! le cherchait directement, et la zone restait hors ligne jusqu'au prochain
//! redémarrage de l'appareil.
//!
//! Le banc tient un faux NanoPlayer : sa description HTTP sur un port
//! éphémère, et un répondeur `M-SEARCH` unicast (port injecté, pas 1900). Le
//! magasin le mémorise à un AUTRE port, fermé.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tune_core::db::zone_repo::ZoneRepo;

const UDN: &str = "uuid:4e616e6f-506c-6179-6572-000000006961";

fn description() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>NanoPlayer</friendlyName>
    <manufacturer>JF Light Industries</manufacturer>
    <modelName>NanoPlayer</modelName>
    <UDN>{UDN}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/AVTransport/control</controlURL>
        <eventSubURL>/AVTransport/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/RenderingControl/control</controlURL>
        <eventSubURL>/RenderingControl/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ConnectionManager/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#
    )
}

/// Sert la description sur `127.0.0.1:<port éphémère>` ; rend ce port.
async fn servir_la_description() -> u16 {
    servir_la_description_quand(Arc::new(AtomicBool::new(true))).await
}

/// Comme [`servir_la_description`], mais l'appareil ne répond que quand
/// `allume` est vrai : sinon la connexion est refermée sans réponse, et la
/// sonde échoue tout de suite — l'appareil injoignable à l'instant de la
/// resonde de démarrage.
async fn servir_la_description_quand(allume: Arc<AtomicBool>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            if !allume.load(Ordering::SeqCst) {
                drop(sock);
                continue;
            }
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let corps = description();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });
    port
}

/// Répond à tout `M-SEARCH` reçu par la `LOCATION` actuelle de l'appareil.
/// Rend le port UDP du répondeur.
async fn repondre_aux_msearch(location: String) -> u16 {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = sock.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, de)) = sock.recv_from(&mut buf).await else {
                return;
            };
            if !buf[..n].starts_with(b"M-SEARCH") {
                continue;
            }
            let reponse = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {location}\r\n\
                 USN: {UDN}::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
                 ST: {UDN}\r\n\r\n"
            );
            let _ = sock.send_to(reponse.as_bytes(), de).await;
        }
    });
    port
}

/// Un port TCP local libre au moment du test : l'ancienne adresse, morte.
fn port_ferme() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// LE scénario de l'issue : la zone existe, hors ligne comme après
/// `reset_zones_offline` ; le magasin garde l'ancien port ; l'appareil est
/// allumé à un autre et ne s'annonce pas. Le démarrage doit le retrouver.
#[tokio::test]
async fn un_renderer_allume_avant_le_serveur_sur_un_autre_port_remet_sa_zone_en_ligne() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let ancienne = format!("http://127.0.0.1:{}/description.xml", port_ferme());
    let actuelle = format!(
        "http://127.0.0.1:{}/description.xml",
        servir_la_description().await
    );
    let port_ssdp = repondre_aux_msearch(actuelle.clone()).await;

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = zones.create("NanoPlayer", Some("dlna"), Some(UDN)).unwrap();
    zones.set_online_by_device(UDN, false).unwrap();
    persist_known_renderer(
        &state.backend,
        UDN,
        &ancienne,
        "NanoPlayer",
        "",
        "JF Light Industries",
        "NanoPlayer",
    );

    let recours = RecoursUnicast {
        port_ssdp,
        budget: std::time::Duration::from_secs(2),
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reregister_known_renderers_avec(&state, recours),
    )
    .await
    .expect("la resonde de démarrage doit se terminer");

    let zone = zones
        .get_by_device_id(UDN)
        .unwrap()
        .expect("zone conservée");
    assert_eq!(zone.id, Some(zone_id), "la MÊME zone, pas un doublon");
    assert!(
        zone.online,
        "#6961 : le renderer allumé avant le serveur, sur un nouveau port, n'a pas été \
         retrouvé au démarrage — sa zone est restée hors ligne"
    );
    assert!(
        state.outputs.lock().await.contains(UDN),
        "la sortie du renderer doit être enregistrée"
    );
    let magasin = load_known_renderers(&state.backend);
    assert_eq!(
        magasin
            .iter()
            .map(|r| r.location.as_str())
            .collect::<Vec<_>>(),
        [actuelle.as_str()],
        "le magasin doit retenir la nouvelle adresse pour le démarrage suivant"
    );
}

/// Garde-fou : le recours ne rattache que l'UDN demandé. Un répondeur qui
/// annonce un AUTRE appareil ne remet pas la zone en ligne.
#[tokio::test]
async fn le_recours_unicast_ignore_un_autre_appareil() {
    let ancienne = format!("http://127.0.0.1:{}/description.xml", port_ferme());
    let actuelle = format!(
        "http://127.0.0.1:{}/description.xml",
        servir_la_description().await
    );
    let port_ssdp = repondre_aux_msearch(actuelle).await;
    let kr = KnownRenderer {
        device_id: "uuid:un-autre-appareil".into(),
        location: ancienne,
        name: "Autre".into(),
        mac: String::new(),
        manufacturer: String::new(),
        model: String::new(),
    };
    let recours = RecoursUnicast {
        port_ssdp,
        budget: std::time::Duration::from_millis(500),
    };
    let sondes =
        sonder_les_renderers_en_parallele(vec![kr], std::time::Duration::from_secs(5), recours)
            .await;
    assert!(
        sondes[0].1.is_none(),
        "un répondeur d'un autre UDN ne doit pas être pris pour l'appareil mémorisé"
    );
}

/// Un renderer injoignable à l'instant de la resonde UNIQUE de démarrage
/// (réseau de la machine pas encore prêt, appareil occupé), qui répond
/// ensuite. Il ne s'annonce pas : sans réévaluation, rien ne le recherchait
/// plus et sa zone restait hors ligne jusqu'au redémarrage de l'appareil.
#[tokio::test]
async fn une_zone_restee_hors_ligne_apres_la_resonde_de_demarrage_est_reevaluee() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let allume = Arc::new(AtomicBool::new(false));
    let location = format!(
        "http://127.0.0.1:{}/description.xml",
        servir_la_description_quand(allume.clone()).await
    );
    // Répondeur SSDP muet : le recours unicast n'obtient rien non plus.
    let muet = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let recours = RecoursUnicast {
        port_ssdp: muet.local_addr().unwrap().port(),
        budget: std::time::Duration::from_millis(300),
    };

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = zones.create("NanoPlayer", Some("dlna"), Some(UDN)).unwrap();
    zones.set_online_by_device(UDN, false).unwrap();
    persist_known_renderer(
        &state.backend,
        UDN,
        &location,
        "NanoPlayer",
        "",
        "JF Light Industries",
        "NanoPlayer",
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reregister_known_renderers_avec(&state, recours),
    )
    .await
    .expect("la resonde de démarrage doit se terminer");
    assert!(
        !zones.get_by_device_id(UDN).unwrap().unwrap().online,
        "préalable du banc : l'appareil injoignable au démarrage laisse la zone hors ligne"
    );

    // L'appareil répond désormais, sans rien annoncer.
    allume.store(true, Ordering::SeqCst);
    let rattaches = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reevaluer_les_zones_hors_ligne_avec(&state, recours),
    )
    .await
    .expect("la réévaluation doit se terminer");

    let zone = zones
        .get_by_device_id(UDN)
        .unwrap()
        .expect("zone conservée");
    assert_eq!(zone.id, Some(zone_id), "la MÊME zone, pas un doublon");
    assert!(
        zone.online,
        "#6961 : le renderer, injoignable à la resonde de démarrage puis joignable, n'a jamais \
         été réévalué — sa zone est restée hors ligne jusqu'au redémarrage de l'appareil"
    );
    assert_eq!(rattaches, 1);
    assert!(
        state.outputs.lock().await.contains(UDN),
        "la sortie du renderer doit être enregistrée"
    );
}

/// Garde-fou : la réévaluation ne sonde QUE les zones DLNA/OpenHome hors
/// ligne. Une zone en ligne, ou d'un autre type, n'est jamais sondée.
#[tokio::test]
async fn la_reevaluation_ne_sonde_que_les_zones_hors_ligne() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    zones.create("NanoPlayer", Some("dlna"), Some(UDN)).unwrap();
    zones
        .create("AirPlay", Some("airplay"), Some("airplay:salon"))
        .unwrap();
    let location = format!("http://127.0.0.1:{}/description.xml", port_ferme());
    persist_known_renderer(&state.backend, UDN, &location, "NanoPlayer", "", "", "");
    persist_known_renderer(
        &state.backend,
        "airplay:salon",
        "http://127.0.0.1:1/x.xml",
        "AirPlay",
        "",
        "",
        "",
    );

    zones.set_online_by_device(UDN, true).unwrap();
    zones.set_online_by_device("airplay:salon", false).unwrap();
    assert!(
        renderers_des_zones_hors_ligne(&state).is_empty(),
        "zone DLNA en ligne et zone non DLNA : rien à sonder"
    );

    zones.set_online_by_device(UDN, false).unwrap();
    let a_sonder: Vec<String> = renderers_des_zones_hors_ligne(&state)
        .into_iter()
        .map(|kr| kr.device_id)
        .collect();
    assert_eq!(a_sonder, [UDN.to_string()]);
}
