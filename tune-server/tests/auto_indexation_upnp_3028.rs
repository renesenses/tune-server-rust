//! Fil du forum 3028 — Tune s'indexait lui-même.
//!
//! Le rapport : sur un Mac sans aucun serveur multimédia, la bibliothèque
//! montre deux fois les mêmes albums — une fois en « Local », une fois sous
//! une source UPnP portant le nom que Tune s'annonce (le nom de la machine,
//! `…fritz.box` derrière une Fritz!Box).
//!
//! Depuis #3786, le MediaServer de Tune figure dans sa propre liste de
//! serveurs (on y parcourt ses dossiers). Mais rien n'empêchait de l'INDEXER :
//! `POST …/indexer` et `POST …/library-source` acceptaient notre propre UDN,
//! et la synchronisation recopiait alors toute la bibliothèque locale en
//! albums `upnp`.
//!
//! Le banc : un vrai ContentDirectory HTTP (SOAP `Browse` réel) qui publie
//! deux pistes. Il est inscrit au registre trois fois, sous trois identités :
//!
//! 1. notre UDN (celui de `AppState.upnp`, persisté en `upnp_udn`) ;
//! 2. un AUTRE UDN — base remise à zéro, ancienne annonce encore en cache —
//!    mais annoncé sur l'une de nos adresses, notre port d'API et notre
//!    chemin de montage `/upnp/description.xml` ;
//! 3. CONTRE-TÉMOIN : un serveur tiers, sur la même machine (127.0.0.1)
//!    mais sur un autre port et un autre chemin — un MinimServer ou un Asset
//!    installé sur le Mac. Il DOIT toujours s'indexer.
//!
//! CONTRE-ÉPREUVE (mesurée) : sans la garde de `indexer`, les deux premiers
//! cas indexent les deux pistes du banc et le témoin rougit.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::discovery::ssdp::MediaServerInfo;
use tune_server::state::AppState;

const UDN_TIERS: &str = "uuid:0b1c2d3e-0000-4000-8000-00000000a3e5";
const UDN_REGENERE: &str = "uuid:5e1f0000-0000-4000-8000-000000003028";

fn echapper(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn item(id: &str, titre: &str, taille: u64, base: &str) -> String {
    format!(
        "<item id=\"{id}\" parentID=\"0\" restricted=\"1\">\
         <dc:title>{titre}</dc:title><dc:creator>Oasis</dc:creator>\
         <upnp:artist>Oasis</upnp:artist><upnp:album>Morning Glory</upnp:album>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res duration=\"0:04:18.000\" size=\"{taille}\" \
         protocolInfo=\"http-get:*:audio/x-flac:*\">{base}/content/{id}.flac</res></item>"
    )
}

fn enveloppe(didl: &str, nombre: usize) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:BrowseResponse><Result>{}</Result>\
         <NumberReturned>{nombre}</NumberReturned>\
         <TotalMatches>{nombre}</TotalMatches><UpdateID>1</UpdateID>\
         </u:BrowseResponse></s:Body></s:Envelope>",
        echapper(&format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{didl}</DIDL-Lite>"
        ))
    )
}

/// Un ContentDirectory réel : deux pistes à la racine, seconde page vide.
async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse"));
    let base_du_service = base.clone();
    let app = Router::new().route(
        "/control",
        post(move |corps: String| {
            let base = base_du_service.clone();
            async move {
                let depart: usize = corps
                    .split_once("<StartingIndex>")
                    .and_then(|(_, r)| r.split_once("</StartingIndex>"))
                    .and_then(|(v, _)| v.trim().parse().ok())
                    .unwrap_or(0);
                if depart > 0 {
                    return enveloppe("", 0);
                }
                let didl = item("p1", "Wonderwall", 31_911_291, &base)
                    + &item("p2", "Champagne Supernova", 54_120_004, &base);
                enveloppe(&didl, 2)
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("{base}/control")
}

fn serveur(id: &str, hote: &str, port: u16, location: &str, controle: &str) -> MediaServerInfo {
    MediaServerInfo {
        id: id.to_string(),
        name: "Mac-de-test.fritz.box".to_string(),
        manufacturer: "Tune".to_string(),
        model: "Tune Server".to_string(),
        location: location.to_string(),
        content_directory_url: controle.to_string(),
        host: hote.to_string(),
        port,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    }
}

async fn appeler(app: &Router, chemin: &str, corps: Option<Value>) -> (StatusCode, Value) {
    let requete = Request::post(chemin).header("content-type", "application/json");
    let requete = match corps {
        Some(c) => requete.body(Body::from(c.to_string())),
        None => requete.body(Body::empty()),
    }
    .unwrap();
    let reponse = app.clone().oneshot(requete).await.expect("routeur");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (
        statut,
        serde_json::from_slice(&octets)
            .unwrap_or_else(|_| json!({ "brut": String::from_utf8_lossy(&octets).to_string() })),
    )
}

fn pistes_upnp_de(etat: &AppState, udn: &str) -> i64 {
    etat.backend
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp' AND source_id LIKE ?",
            &[&format!("{udn}|%")],
        )
        .unwrap()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(-1)
}

#[tokio::test(flavor = "multi_thread")]
async fn tune_ne_s_indexe_pas_lui_meme_mais_un_serveur_tiers_si() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let notre_udn = etat.upnp.as_ref().expect("état UPnP local").uuid.clone();
    let port_api = etat.config.port;
    let controle = lever_le_banc().await;
    let notre_location = tune_core::upnp_server::advert_location("127.0.0.1", port_api);

    // 1. Notre UDN. 2. Un autre UDN, sur notre adresse et notre montage.
    //    Pour le Mac à plusieurs interfaces, chacune de nos adresses IPv4.
    let mut a_nous = vec![
        serveur(
            &notre_udn,
            "127.0.0.1",
            port_api,
            &notre_location,
            &controle,
        ),
        serveur(
            UDN_REGENERE,
            "127.0.0.1",
            port_api,
            &notre_location,
            &controle,
        ),
    ];
    for (i, ip) in tune_core::discovery::ssdp::local_ipv4_addresses()
        .iter()
        .enumerate()
    {
        let ip = ip.to_string();
        a_nous.push(serveur(
            &format!("uuid:5e1f0000-0000-4000-8000-0000000031{i:02}"),
            &ip,
            port_api,
            &tune_core::upnp_server::advert_location(&ip, port_api),
            &controle,
        ));
    }
    // 3. Le contre-témoin : même machine, autre port, autre chemin.
    let tiers = serveur(
        UDN_TIERS,
        "127.0.0.1",
        26125,
        "http://127.0.0.1:26125/DeviceDescription.xml",
        &controle,
    );
    {
        let mut registre = etat.media_servers.lock().await;
        for s in a_nous.iter().chain(std::iter::once(&tiers)) {
            registre.insert(s.id.clone(), s.clone());
        }
    }
    let app = tune_server::routes::router(etat.clone());

    for s in &a_nous {
        let (statut, corps) = appeler(
            &app,
            &format!("/api/v1/network/media-servers/{}/indexer", s.id),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{}: {corps}", s.id);
        assert_eq!(
            pistes_upnp_de(&etat, &s.id),
            0,
            "Tune ne doit pas recopier sa bibliothèque en albums upnp ({} sur {}) — {corps}",
            s.id,
            s.host
        );
        assert_eq!(corps["indexe"], json!(false), "{corps}");
        assert_eq!(corps["code"], json!("this_server"), "{corps}");

        // L'abonnement (synchronisation durable) est refusé AVANT d'écrire.
        let (statut, corps) = appeler(
            &app,
            &format!("/api/v1/network/media-servers/{}/library-source", s.id),
            Some(json!({ "container": "0" })),
        )
        .await;
        assert_eq!(statut, StatusCode::CONFLICT, "{}: {corps}", s.id);
        assert_eq!(corps["code"], json!("this_server"), "{corps}");
    }
    let sources = etat
        .backend
        .query_one("SELECT COUNT(*) FROM upnp_library_sources", &[])
        .unwrap()
        .and_then(|r| r[0].as_i64());
    assert_eq!(sources, Some(0), "aucune source ne doit viser ce Tune");

    // CONTRE-TÉMOIN : le serveur tiers s'indexe toujours.
    let (statut, corps) = appeler(
        &app,
        &format!("/api/v1/network/media-servers/{UDN_TIERS}/indexer"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["indexe"], json!(true), "{corps}");
    assert_eq!(
        pistes_upnp_de(&etat, UDN_TIERS),
        2,
        "un vrai serveur UPnP tiers doit rester indexable — {corps}"
    );
}
