//! #6934 — `QobuzService::get_label` : UN `label/get` par page, le nom et le
//! logo du label, `total` tel que Qobuz l'annonce, sans les précommandes ni
//! les albums retirés.

use super::*;
use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use std::collections::HashMap as Carte;
use std::sync::{Arc, Mutex};

const PAGE_ECM: &str = include_str!("../../tests/fixtures/qobuz/label_get_albums_page_6934.json");

async fn qobuz_simule() -> (QobuzService, Arc<Mutex<Vec<Carte<String, String>>>>) {
    let journal = Arc::new(Mutex::new(Vec::new()));
    let j = journal.clone();
    let app = Router::new().route(
        "/label/get",
        get(move |Query(q): Query<Carte<String, String>>| {
            j.lock().unwrap().push(q);
            async move { Json(serde_json::from_str::<serde_json::Value>(PAGE_ECM).unwrap()) }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        QobuzService::avec_base_forcee(format!("http://{adresse}")),
        journal,
    )
}

#[tokio::test]
async fn une_page_du_label_est_un_seul_label_get() {
    let (svc, journal) = qobuz_simule().await;
    let page = svc.get_label("1150", 4, 8).await.expect("Qobuz simulé");
    let appels = journal.lock().unwrap().clone();
    assert_eq!(appels.len(), 1, "une page, une requête");
    assert_eq!(appels[0]["label_id"], "1150");
    assert_eq!(appels[0]["extra"], "albums");
    assert_eq!(appels[0]["limit"], "4");
    assert_eq!(appels[0]["offset"], "8");
    assert_eq!(page.offset, 8);
    assert_eq!(page.limit, 4);
}

#[tokio::test]
async fn la_page_porte_le_nom_le_logo_et_le_total_du_label() {
    let (svc, _) = qobuz_simule().await;
    let page = svc.get_label("1150", 4, 0).await.expect("Qobuz simulé");
    assert_eq!(page.id, "1150");
    assert_eq!(page.name, "ECM");
    assert_eq!(
        page.image_url.as_deref(),
        Some("https://static.qobuz.test/images/labels/1150_large.jpg")
    );
    assert_eq!(
        page.total, 9,
        "le total annoncé par Qobuz, pas celui de la page"
    );
}

#[tokio::test]
async fn la_page_ecarte_les_precommandes_et_les_albums_non_ecoutables() {
    let (svc, _) = qobuz_simule().await;
    let page = svc.get_label("1150", 4, 0).await.expect("Qobuz simulé");
    let ids: Vec<&str> = page.albums.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["0602537344315", "0042284334028"]);
    assert_eq!(page.albums[0].artist, "Jan Garbarek");
    assert_eq!(page.albums[0].artist_id.as_deref(), Some("20"));
    assert_eq!(page.albums[0].year, Some(2002));
}
