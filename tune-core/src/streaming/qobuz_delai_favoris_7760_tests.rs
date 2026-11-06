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

/// Un Qobuz qui sert l'adresse de lecture d'une piste au bout de `attente`.
async fn qobuz_lent_pour_la_lecture(attente: Duration) -> String {
    let app = Router::new().route(
        "/track/getFileUrl",
        get(move || async move {
            tokio::time::sleep(attente).await;
            Json(json!({
                "url": "https://flux.essai/piste.flac",
                "mime_type": "audio/flac",
                "sampling_rate": 96,
                "bit_depth": 24,
                "format_id": 27
            }))
        }),
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

/// La borne de #7760 ne vaut QUE pour les listes du compte : l'adresse de
/// lecture d'une piste attend encore au-delà de 3 s. Bornée, une adresse
/// principale lente ferait descendre l'échelle de qualité (27 → 7 → 6) ou
/// échouer la lecture, alors que Qobuz allait répondre.
#[tokio::test]
async fn l_adresse_de_lecture_d_une_piste_n_est_pas_bornee_7760() {
    let attente = DELAI_REPONSE_LISTE_DU_COMPTE + Duration::from_secs(1);
    let bases = BasesQobuz {
        api: qobuz_lent_pour_la_lecture(attente).await,
        proxy: adresse_muette().await,
        config_distante: adresse_muette().await,
    };
    let mut svc = QobuzService::avec_bases_de_test(
        String::from("app-id-essai"),
        String::from("secret-essai"),
        bases,
    );
    svc.user_auth_token = Some(String::from("jeton-essai"));

    let flux = svc.fetch_track_url("123", "27").await.unwrap_or_else(|e| {
        panic!(
            "#7760 : l'adresse de lecture d'une piste a été abandonnée alors que Qobuz \
             répondait en {} ms — la borne des listes du compte ne doit pas la toucher : {e}",
            attente.as_millis()
        )
    });
    assert_eq!(flux.url, "https://flux.essai/piste.flac");
    assert_eq!(flux.quality.bit_depth, 24, "la qualité demandée est servie");
}
