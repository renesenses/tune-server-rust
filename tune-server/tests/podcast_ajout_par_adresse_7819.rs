//! Ajouter un podcast par l'adresse de son flux (#7819).
//!
//! `POST /api/v1/podcasts/subscriptions` avec seulement `feed_url` : le
//! serveur lit le flux, le valide et remplit l'abonnement lui-même. Le banc
//! sert un vrai flux RSS et une page HTML sur la boucle locale et appelle la
//! route par son URL, à travers le routeur complet :
//!
//!   1. un flux RSS s'abonne avec le titre, l'auteur, l'image et la
//!      description du flux ;
//!   2. une page HTML est refusée (422, `podcast_pas_un_flux`, message
//!      traduit) et rien n'est écrit en base ;
//!   3. `feed://…` puis `http://…` du même flux ne donnent qu'un abonnement,
//!      et le second appel ne relit pas le flux ;
//!   4. une adresse injoignable est refusée (502), une adresse qui n'est pas
//!      web aussi (400).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use serde_json::{Value, json};
use tower::ServiceExt;

struct FauxHebergeur {
    port: u16,
    lectures_du_flux: Arc<AtomicUsize>,
}

async fn faux_hebergeur() -> FauxHebergeur {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let lectures_du_flux = Arc::new(AtomicUsize::new(0));
    let compteur = lectures_du_flux.clone();
    let flux = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
<channel>
<title>Le Podcast du Banc</title>
<itunes:author>Équipe Tune</itunes:author>
<description>Un flux servi en local.</description>
<itunes:image href="http://127.0.0.1:{port}/couverture.jpg"/>
<item><title>Épisode 1</title><enclosure url="http://127.0.0.1:{port}/ep1.mp3" type="audio/mpeg"/></item>
</channel>
</rss>"#
    );
    let app = Router::new()
        .route(
            "/flux.xml",
            get(move || {
                compteur.fetch_add(1, Ordering::SeqCst);
                let flux = flux.clone();
                async move { ([(header::CONTENT_TYPE, "application/rss+xml")], flux) }
            }),
        )
        .route(
            "/page.html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<!DOCTYPE html><html><head><title>Mon podcast</title></head><body>RSS</body></html>",
                )
            }),
        );
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    FauxHebergeur {
        port,
        lectures_du_flux,
    }
}

fn app() -> Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn ajouter(app: &Router, adresse: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post("/api/v1/podcasts/subscriptions")
                .header("Content-Type", "application/json")
                .header("Accept-Language", "fr-FR")
                .body(Body::from(json!({ "feed_url": adresse }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

async fn abonnements(app: &Router) -> Vec<Value> {
    let reponse = app
        .clone()
        .oneshot(
            Request::get("/api/v1/podcasts/subscriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&octets)
        .unwrap()
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn un_flux_rss_s_abonne_avec_le_titre_et_l_image_du_flux() {
    let hebergeur = faux_hebergeur().await;
    let app = app();
    let adresse = format!("http://127.0.0.1:{}/flux.xml", hebergeur.port);

    let (status, corps) = ajouter(&app, &adresse).await;

    assert_eq!(status, StatusCode::CREATED, "{corps}");
    assert_eq!(corps["created"], true, "{corps}");
    assert_eq!(
        corps["title"], "Le Podcast du Banc",
        "le titre doit venir du flux : {corps}"
    );
    let image = format!("http://127.0.0.1:{}/couverture.jpg", hebergeur.port);
    assert_eq!(corps["image_url"], image.as_str(), "{corps}");
    assert!(corps["id"].as_i64().is_some(), "{corps}");

    let liste = abonnements(&app).await;
    assert_eq!(liste.len(), 1, "{liste:?}");
    assert_eq!(liste[0]["title"], "Le Podcast du Banc");
    assert_eq!(liste[0]["author"], "Équipe Tune");
    assert_eq!(liste[0]["image_url"], image.as_str());
    assert_eq!(liste[0]["description"], "Un flux servi en local.");
    assert_eq!(liste[0]["feed_url"], adresse.as_str());
}

#[tokio::test]
async fn une_page_html_est_refusee_et_rien_n_est_ecrit() {
    let hebergeur = faux_hebergeur().await;
    let app = app();

    let (status, corps) = ajouter(
        &app,
        &format!("http://127.0.0.1:{}/page.html", hebergeur.port),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "une page HTML n'est pas un flux : {corps}"
    );
    assert_eq!(corps["code"], "podcast_pas_un_flux", "{corps}");
    assert!(
        corps["error"]
            .as_str()
            .is_some_and(|m| m.contains("flux de podcast")),
        "le message doit être traduit dans la langue demandée : {corps}"
    );
    assert!(abonnements(&app).await.is_empty(), "aucune ligne attendue");
}

#[tokio::test]
async fn le_meme_flux_par_feed_puis_par_http_ne_s_abonne_qu_une_fois() {
    let hebergeur = faux_hebergeur().await;
    let app = app();

    let (premier_status, premier) = ajouter(
        &app,
        &format!("feed://127.0.0.1:{}/flux.xml", hebergeur.port),
    )
    .await;
    assert_eq!(premier_status, StatusCode::CREATED, "{premier}");
    assert_eq!(
        premier["feed_url"],
        format!("http://127.0.0.1:{}/flux.xml", hebergeur.port).as_str(),
        "feed:// est enregistré en http:// : {premier}"
    );

    let (second_status, second) = ajouter(
        &app,
        &format!("  http://127.0.0.1:{}/flux.xml ", hebergeur.port),
    )
    .await;

    assert_eq!(
        second_status,
        StatusCode::OK,
        "un flux déjà suivi n'est pas une création : {second}"
    );
    assert_eq!(second["created"], false, "{second}");
    assert_eq!(second["id"], premier["id"], "{second}");
    assert_eq!(second["title"], "Le Podcast du Banc", "{second}");
    assert_eq!(abonnements(&app).await.len(), 1, "un seul abonnement");
    assert_eq!(
        hebergeur.lectures_du_flux.load(Ordering::SeqCst),
        1,
        "le flux déjà suivi n'est pas relu"
    );
}

#[tokio::test]
async fn une_adresse_injoignable_ou_qui_n_est_pas_web_est_refusee() {
    let app = app();
    // Un port libéré aussitôt : la connexion est refusée.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let (status, corps) = ajouter(&app, &format!("http://127.0.0.1:{port}/flux.xml")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{corps}");
    assert_eq!(corps["code"], "podcast_flux_injoignable", "{corps}");

    let (status, corps) = ajouter(&app, "ftp://exemple.test/flux.xml").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["code"], "podcast_adresse_invalide", "{corps}");

    assert!(abonnements(&app).await.is_empty());
}
