//! #7695 — une photo d'un serveur multimédia n'entre pas dans `tracks`.
//!
//! # Le défaut
//!
//! Le partage multimédia de Windows d'un portable publie aussi les photos du
//! poste. La récolte (`recolter`, `routes/indexation_upnp.rs`) retenait chaque
//! `<item>` titré et doté d'une URL : chaque photo DNG devenait une « piste »
//! sans artiste, en tête de la bibliothèque et d'Oxygen. Quand un item n'a
//! qu'une ressource, `select_best_res` la garde même si c'est une image : rien
//! ne l'arrêtait.
//!
//! # Le banc
//!
//! Un ContentDirectory qui publie, sous sa racine :
//!
//! - `photos` : une photo `object.item.imageItem.photo` en `.dng`
//!   (`image/x-adobe-dng`) ;
//! - `musique` : une piste `object.item.audioItem.musicTrack`, et une piste
//!   aux métadonnées pauvres — classe générique `object.item`, MIME
//!   `audio/flac`, ni artiste ni album.
//!
//! On passe par la route montée `POST …/indexer`, puis on lit la base.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

const UDN: &str = "uuid:9e933823-7695-7695-7695-000000007695";

fn enveloppe_soap(didl: &str, nombre: usize) -> axum::response::Response<String> {
    let echappe = format!("<DIDL-Lite>{didl}</DIDL-Lite>")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    axum::response::Response::builder()
        .header("content-type", "text/xml")
        .body(format!(
            "<?xml version=\"1.0\"?><s:Envelope \
             xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{echappe}</Result><NumberReturned>{nombre}</NumberReturned>\
             <TotalMatches>{nombre}</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"
        ))
        .unwrap()
}

/// Une photo telle que la publie le partage multimédia de Windows.
fn photo_dng(base: &str) -> String {
    format!(
        "<item id=\"p-1\" parentID=\"photos\" restricted=\"1\"><dc:title>IMG_0042</dc:title>\
         <upnp:class>object.item.imageItem.photo</upnp:class>\
         <res protocolInfo=\"http-get:*:image/x-adobe-dng:*\" size=\"25165824\" \
         resolution=\"6000x4000\">{base}/photos/IMG_0042.dng</res></item>"
    )
}

fn piste_musicale(base: &str) -> String {
    format!(
        "<item id=\"m-1\" parentID=\"musique\"><dc:title>Piste audio</dc:title>\
         <upnp:artist>Artiste</upnp:artist><upnp:album>Album</upnp:album>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:01.000\" \
         size=\"1000001\">{base}/musique/1.flac</res></item>"
    )
}

/// Un vrai fichier audio aux métadonnées pauvres : classe générique, rien
/// d'autre que le MIME pour dire que c'est du son.
fn piste_pauvre(base: &str) -> String {
    format!(
        "<item id=\"m-2\" parentID=\"musique\"><dc:title>Piste pauvre</dc:title>\
         <upnp:class>object.item</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\">{base}/res/2</res></item>"
    )
}

async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse liée"));
    let pour_le_service = base.clone();
    let app = Router::new().route(
        "/control",
        post(move |corps: String| {
            let base = pour_le_service.clone();
            async move {
                let object_id = corps
                    .split_once("<ObjectID>")
                    .and_then(|(_, r)| r.split_once("</ObjectID>"))
                    .map(|(v, _)| v.to_string())
                    .unwrap_or_default();
                let depart: usize = corps
                    .split_once("<StartingIndex>")
                    .and_then(|(_, r)| r.split_once("</StartingIndex>"))
                    .and_then(|(v, _)| v.trim().parse().ok())
                    .unwrap_or(0);
                if depart > 0 {
                    return enveloppe_soap("", 0);
                }
                match object_id.as_str() {
                    "0" => enveloppe_soap(
                        "<container id=\"photos\" parentID=\"0\" childCount=\"1\">\
                         <dc:title>Images</dc:title></container>\
                         <container id=\"musique\" parentID=\"0\" childCount=\"2\">\
                         <dc:title>Musique</dc:title></container>",
                        2,
                    ),
                    "photos" => enveloppe_soap(&photo_dng(&base), 1),
                    "musique" => enveloppe_soap(
                        &format!("{}{}", piste_musicale(&base), piste_pauvre(&base)),
                        2,
                    ),
                    _ => enveloppe_soap("", 0),
                }
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("{base}/control")
}

async fn inscrire(etat: &AppState, control_url: &str) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: UDN.to_string(),
        name: "Portable Windows".to_string(),
        manufacturer: "Microsoft Corporation".to_string(),
        model: "Windows Media Player Sharing".to_string(),
        location: control_url.to_string(),
        content_directory_url: control_url.to_string(),
        host: "127.0.0.1".to_string(),
        port: 0,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    };
    etat.media_servers
        .lock()
        .await
        .insert(UDN.to_string(), info);
}

async fn indexer(app: &Router) -> Value {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/network/media-servers/{UDN}/indexer"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("routeur en échec");
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    let corps: Value = serde_json::from_slice(&octets).expect("JSON");
    assert_eq!(corps["indexe"], json!(true), "banc introuvable : {corps}");
    corps
}

fn titres_upnp(etat: &AppState) -> Vec<String> {
    let mut t: Vec<String> = etat
        .backend
        .query_many("SELECT title FROM tracks WHERE source = 'upnp'", &[])
        .expect("lecture des pistes")
        .iter()
        .filter_map(|r| r[0].as_str().map(str::to_string))
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn i7695_une_photo_dng_d_un_serveur_media_n_entre_pas_dans_les_pistes() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat.clone());

    let corps = indexer(&app).await;

    let titres = titres_upnp(&etat);
    assert!(
        !titres.iter().any(|t| t == "IMG_0042"),
        "la photo DNG IMG_0042 (object.item.imageItem.photo, image/x-adobe-dng) \
         a été indexée comme une piste — pistes {titres:?}, corps {corps}"
    );
    assert_eq!(
        titres,
        vec!["Piste audio".to_string(), "Piste pauvre".to_string()],
        "seules les deux pistes audio doivent entrer dans `tracks`, y compris \
         celle de classe générique `object.item` en audio/flac — corps {corps}"
    );
    assert_eq!(
        corps["pistes"]["ecartees_non_audio"],
        json!(1),
        "la réponse doit dire combien de photos ou vidéos ont été écartées — corps {corps}"
    );
    assert_eq!(
        corps["complet"],
        json!(true),
        "écarter une photo n'est pas une passe incomplète : sinon la \
         synchronisation n'autoriserait plus aucun retrait — corps {corps}"
    );

    // Deux passes : toujours deux pistes, aucune photo revenue.
    let _ = indexer(&app).await;
    assert_eq!(titres_upnp(&etat).len(), 2);
}
