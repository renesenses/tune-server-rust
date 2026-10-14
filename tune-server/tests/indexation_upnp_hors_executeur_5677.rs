//! #5677 — l'écriture d'une passe d'indexation UPnP ne doit pas tenir un fil
//! de l'exécuteur.
//!
//! # Le défaut
//!
//! `indexation_upnp::ecrire_avec_pochettes` est `async`, mais elle appelait
//! `ecrire` directement : une boucle **synchrone** qui, pour chaque piste du
//! serveur distant, lit et écrit SQLite (artiste, album, piste, métadonnées).
//! La synchronisation horaire des sources UPnP (`synchronisation_upnp::start`,
//! une passe par heure et par source) repasse sur TOUTES les pistes à chaque
//! fois. Pendant toute cette boucle, le fil de l'exécuteur qui la porte ne sert
//! plus rien d'autre : ni le battement du chien de garde, ni le corps HTTP du
//! flux interne de la sortie locale. Sur une machine à un seul fil de travail,
//! c'est tout le serveur qui s'arrête.
//!
//! # Ce que l'épreuve mesure
//!
//! Un exécuteur à **un** fil de travail (le cas de l'installation du
//! signalement, `cpu_count: 1`), un banc ContentDirectory qui publie
//! `PISTES_DU_BANC` pistes par pages de 200, et un battement qui note son plus
//! long retard pendant une **seconde** passe (celle de la synchronisation
//! horaire : toutes les pistes existent déjà et sont mises à jour). La base est
//! un vrai fichier SQLite, pas `:memory:`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

const UDN: &str = "uuid:5677-banc-hors-executeur";

/// Assez de pistes pour que la boucle d'écriture dure, en build de test, bien
/// plus que le retard toléré du battement.
const PISTES_DU_BANC: usize = 2_000;
/// La taille de page que Tune demande (`RequestedCount`).
const PAGE: usize = 200;
/// Le plus long retard toléré du battement pendant la passe. Les seuls travaux
/// synchrones qui restent sur l'exécuteur sont l'analyse d'une page DIDL de 200
/// éléments et le service du banc : quelques millisecondes.
const RETARD_TOLERE: Duration = Duration::from_millis(500);

fn enveloppe_soap(didl: &str, rendus: usize, total: usize) -> axum::response::Response<String> {
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
             <Result>{echappe}</Result><NumberReturned>{rendus}</NumberReturned>\
             <TotalMatches>{total}</TotalMatches><UpdateID>1</UpdateID>\
             </u:BrowseResponse></s:Body></s:Envelope>"
        ))
        .unwrap()
}

fn didl_piste(index: usize, base: &str) -> String {
    format!(
        "<item id=\"piste-{index}\" parentID=\"axe\"><dc:title>Piste {index}</dc:title>\
         <upnp:artist>Artiste {}</upnp:artist><upnp:album>Album {}</upnp:album>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:00.000\" \
         size=\"{}\">{base}/p{index}.flac</res></item>",
        index % 50,
        index % 200,
        1_000_000 + index
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
                let champ = |nom: &str| {
                    corps
                        .split_once(&format!("<{nom}>"))
                        .and_then(|(_, r)| r.split_once(&format!("</{nom}>")))
                        .map(|(v, _)| v.trim().to_string())
                        .unwrap_or_default()
                };
                let depart: usize = champ("StartingIndex").parse().unwrap_or(0);
                match champ("ObjectID").as_str() {
                    "0" if depart == 0 => enveloppe_soap(
                        "<container id=\"axe\" parentID=\"0\"><dc:title>Tout</dc:title></container>",
                        1,
                        1,
                    ),
                    "axe" => {
                        let fin = (depart + PAGE).min(PISTES_DU_BANC);
                        let didl: String = (depart..fin).map(|i| didl_piste(i, &base)).collect();
                        enveloppe_soap(&didl, fin.saturating_sub(depart), PISTES_DU_BANC)
                    }
                    _ => enveloppe_soap("", 0, 0),
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
        name: "Banc 5677".to_string(),
        manufacturer: "Banc".to_string(),
        model: "ContentDirectory".to_string(),
        location: control_url.to_string(),
        content_directory_url: control_url.to_string(),
        host: "127.0.0.1".to_string(),
        port: 0,
        last_seen: Instant::now(),
        max_age: Duration::from_secs(1800),
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
    serde_json::from_slice(&octets).expect("corps JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn i5677_la_passe_horaire_ne_tient_pas_l_executeur() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let base = dossier.path().join("tune.db");
    let etat = AppState::new(base.to_str().unwrap(), 0, Default::default()).expect("état serveur");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat);

    // Première passe : les pistes entrent.
    let premiere = indexer(&app).await;
    assert_eq!(
        premiere["pistes"]["distinctes"].as_u64(),
        Some(PISTES_DU_BANC as u64),
        "le banc doit entrer en entier : {premiere}"
    );

    // Seconde passe, celle de la synchronisation horaire : tout est mis à
    // jour. Le battement tourne pendant ce temps sur le MÊME exécuteur.
    let pire_retard_us = Arc::new(AtomicU64::new(0));
    let arret = Arc::new(AtomicBool::new(false));
    let battement = {
        let pire = pire_retard_us.clone();
        let arret = arret.clone();
        tokio::spawn(async move {
            while !arret.load(Ordering::Relaxed) {
                let avant = Instant::now();
                tokio::time::sleep(Duration::from_millis(2)).await;
                let retard = avant.elapsed().as_micros() as u64;
                pire.fetch_max(retard, Ordering::Relaxed);
            }
        })
    };
    // Laisser le battement prendre son rythme avant la passe.
    tokio::time::sleep(Duration::from_millis(20)).await;
    // La passe tourne dans une TÂCHE, donc sur le fil de travail, comme la
    // route et la boucle horaire en production. Le corps d'un
    // `#[tokio::test]` tourne, lui, sur le fil de `block_on`, hors des fils de
    // travail : appelée d'ici, la passe ne pourrait pas gêner le battement et
    // l'épreuve serait verte contre rien.
    let debut = Instant::now();
    let pour_la_tache = app.clone();
    let seconde = tokio::spawn(async move { indexer(&pour_la_tache).await })
        .await
        .expect("passe d'indexation");
    let duree = debut.elapsed();
    arret.store(true, Ordering::Relaxed);
    battement.await.expect("battement");

    assert_eq!(
        seconde["pistes"]["mises_a_jour"].as_u64(),
        Some(PISTES_DU_BANC as u64),
        "la seconde passe doit mettre à jour toutes les pistes : {seconde}"
    );
    let pire = Duration::from_micros(pire_retard_us.load(Ordering::Relaxed));
    eprintln!("i5677 : passe de {duree:?}, pire retard du battement {pire:?}");
    assert!(
        pire < RETARD_TOLERE,
        "pendant une passe d'indexation de {duree:?}, le battement de l'exécuteur \
         est resté arrêté {pire:?} (toléré : {RETARD_TOLERE:?}) : l'écriture des \
         pistes tient un fil de l'exécuteur"
    );
}
