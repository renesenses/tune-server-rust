//! #7760 — un compte Qobuz SANS favoris doit rendre sa liste vide vite.
//!
//! Dans Streaming › Qobuz › Editorial, « Favourite albums » et « My playlists »
//! d'un compte vide affichaient « This widget could not be loaded » : le
//! widget abandonne à 8 s, et une adresse Qobuz qui accepte la connexion sans
//! jamais répondre retenait chaque lecture jusqu'à 45 s avant le repli.
//!
//! Le faux Qobuz a deux adresses : la principale accepte la connexion et se
//! tait, celle de secours sert un compte vide. Aucun appel ne sort de la
//! machine.

use super::*;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

/// Ce que le widget du client web attend avant d'afficher « délai »
/// (`DELAI_MS` de `PageWidgets.svelte`).
const DELAI_DU_WIDGET: Duration = Duration::from_secs(8);

/// Une adresse qui accepte les connexions et ne répond jamais.
async fn adresse_muette() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let mut gardees = Vec::new();
        while let Ok((socket, _)) = ecoute.accept().await {
            // Garder la socket ouverte : le client attend une réponse.
            gardees.push(socket);
        }
    });
    format!("http://{adresse}")
}

/// Un Qobuz qui sert un compte sans aucun favori ni playlist.
async fn qobuz_compte_vide() -> String {
    let app = Router::new()
        .route(
            "/favorite/getUserFavorites",
            get(|| async { Json(json!({"albums": {"items": [], "total": 0}})) }),
        )
        .route(
            "/playlist/getUserPlaylists",
            get(|| async { Json(json!({"playlists": {"items": [], "total": 0}})) }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

async fn service_principale_muette() -> QobuzService {
    let bases = BasesQobuz {
        api: adresse_muette().await,
        proxy: qobuz_compte_vide().await,
        config_distante: adresse_muette().await,
    };
    let mut svc = QobuzService::avec_bases_de_test(
        String::from("app-id-essai"),
        String::from("secret-essai"),
        bases,
    );
    svc.user_auth_token = Some(String::from("jeton-essai"));
    svc
}

#[tokio::test]
async fn favoris_albums_d_un_compte_vide_arrivent_avant_le_delai_du_widget_7760() {
    let svc = service_principale_muette().await;
    let debut = Instant::now();
    let albums = tokio::time::timeout(DELAI_DU_WIDGET, svc.get_user_favorites_dated("albums"))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "#7760 : les albums favoris d'un compte vide ne sont pas arrivés en {} s \
                 alors que l'adresse de secours répond — le widget afficherait « délai »",
                DELAI_DU_WIDGET.as_secs()
            )
        })
        .expect("l'adresse de secours sert la liste");
    assert_eq!(
        albums,
        Some(Vec::new()),
        "un compte vide rend une liste vide"
    );
    assert!(debut.elapsed() < DELAI_DU_WIDGET);
}

#[tokio::test]
async fn playlists_d_un_compte_vide_arrivent_avant_le_delai_du_widget_7760() {
    let svc = service_principale_muette().await;
    let playlists = tokio::time::timeout(DELAI_DU_WIDGET, svc.get_user_playlists())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "#7760 : les playlists d'un compte vide ne sont pas arrivées en {} s \
                 alors que l'adresse de secours répond — le widget afficherait « délai »",
                DELAI_DU_WIDGET.as_secs()
            )
        })
        .expect("l'adresse de secours sert la liste");
    assert!(playlists.is_empty(), "un compte vide rend une liste vide");
}

/// Deux tentatives bornées tiennent ensemble sous le délai du widget, avec
/// de la marge pour le corps et la route.
#[test]
fn deux_tentatives_tiennent_sous_le_delai_du_widget_7760() {
    assert!(DELAI_REPONSE_LECTURE * 2 < DELAI_DU_WIDGET - Duration::from_secs(1));
}
