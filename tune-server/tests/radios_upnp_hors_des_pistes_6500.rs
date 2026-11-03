//! #6500 — une station de radio d'un serveur multimédia n'entre pas dans
//! `tracks`.
//!
//! # Le défaut
//!
//! Relevé sur le `.18` le 19/10/2026 : les 31 premières lignes de
//! Bibliothèque > Pistes étaient des stations de radio (FIP, KEXP, …), toutes
//! `source = 'upnp'`, sans artiste — donc triées en tête —, sans album, durée
//! nulle. Leur instantané : `upnp_object_id = radio/<n>`,
//! `upnp_res_url = http://<autre Tune>/api/v1/radios/<n>/audio.wav`. La source
//! « Tune Server » d'un autre Tune avait été ajoutée depuis sa racine, et la
//! récolte (`recolter`, `routes/indexation_upnp.rs`) retenait chaque `<item>`
//! titré et doté d'une URL, rayon Radio compris.
//!
//! # Le banc
//!
//! Un ContentDirectory qui publie, sous sa racine :
//!
//! - `albums` : deux vraies pistes ;
//! - `radios` : deux stations **à la façon d'un Tune** — `id="radio/<n>"`,
//!   `parentID="radios"`, et la classe `musicTrack` que ce rayon publie au
//!   `Browse` (`RADIO_CLASSE_BROWSE`). La classe ne les trahit donc pas ;
//! - `flux` : une station **à la façon normalisée**, classe
//!   `object.item.audioItem.audioBroadcast`, sous un identifiant quelconque.
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

const UDN: &str = "uuid:9e933823-6500-6500-6500-000000006500";

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

fn piste(n: usize, base: &str) -> String {
    format!(
        "<item id=\"track/{n}\" parentID=\"albums\"><dc:title>Piste {n}</dc:title>\
         <upnp:artist>Artiste</upnp:artist><upnp:album>Album</upnp:album>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:0{n}.000\" \
         size=\"{}\">{base}/api/v1/library/tracks/{n}/audio</res></item>",
        1_000_000 + n
    )
}

/// Une station telle que le rayon Radio d'un Tune la publie au `Browse` :
/// `musicTrack`, sans durée ni taille (`live_stream`), WAV.
fn station_de_tune(n: usize, nom: &str, base: &str) -> String {
    format!(
        "<item id=\"radio/{n}\" parentID=\"radios\" restricted=\"1\"><dc:title>{nom}</dc:title>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/wav:*\">{base}/api/v1/radios/{n}/audio.wav</res></item>"
    )
}

fn station_normalisee(base: &str) -> String {
    format!(
        "<item id=\"s-1\" parentID=\"flux\"><dc:title>Station normalisée</dc:title>\
         <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/mpeg:*\">{base}/live.mp3</res></item>"
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
                        "<container id=\"albums\" parentID=\"0\" childCount=\"2\">\
                         <dc:title>Albums</dc:title></container>\
                         <container id=\"radios\" parentID=\"0\" childCount=\"2\">\
                         <dc:title>Radio</dc:title></container>\
                         <container id=\"flux\" parentID=\"0\" childCount=\"1\">\
                         <dc:title>Flux</dc:title></container>",
                        3,
                    ),
                    "albums" => {
                        enveloppe_soap(&format!("{}{}", piste(1, &base), piste(2, &base)), 2)
                    }
                    "radios" => enveloppe_soap(
                        &format!(
                            "{}{}",
                            station_de_tune(74, "FIP", &base),
                            station_de_tune(75, "KEXP", &base)
                        ),
                        2,
                    ),
                    "flux" => enveloppe_soap(&station_normalisee(&base), 1),
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
        name: "Tune Server".to_string(),
        manufacturer: "Mozaiklabs".to_string(),
        model: "Tune".to_string(),
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
        .query_many(
            "SELECT title FROM tracks WHERE source = 'upnp' ORDER BY title",
            &[],
        )
        .expect("lecture des pistes")
        .iter()
        .filter_map(|r| r[0].as_str().map(str::to_string))
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn i6500_les_stations_d_un_serveur_media_n_entrent_pas_dans_les_pistes() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat.clone());

    let corps = indexer(&app).await;

    assert_eq!(
        titres_upnp(&etat),
        vec!["Piste 1".to_string(), "Piste 2".to_string()],
        "seules les deux vraies pistes doivent entrer dans `tracks` ; les stations \
         FIP, KEXP (rayon Radio d'un Tune) et la station `audioBroadcast` n'y ont \
         pas leur place — corps {corps}"
    );
    assert_eq!(
        corps["pistes"]["ecartees_radios"],
        json!(3),
        "la réponse doit dire combien de stations ont été écartées — corps {corps}"
    );
    assert_eq!(
        corps["complet"],
        json!(true),
        "écarter une station n'est pas une passe incomplète : sinon la \
         synchronisation n'autoriserait plus aucun retrait — corps {corps}"
    );

    // Deux passes : toujours deux pistes, aucune station revenue.
    let _ = indexer(&app).await;
    assert_eq!(titres_upnp(&etat).len(), 2);
}
