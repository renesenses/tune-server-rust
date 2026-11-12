//! Février 2026 — Spotify a retiré `PUT`/`DELETE /me/tracks`, `/me/albums`
//! et `/me/following` : ajouter ou retirer un favori passe par
//! `/me/library?uris=…`. Le cœur « favori » de Tune répondait 403.
//!
//! Doublure locale de l'API : aucune requête ne part vers Spotify.

use super::*;
use std::sync::{Arc, Mutex};

type Journal = Arc<Mutex<Vec<(String, String)>>>;

/// Un faux Spotify qui n'accepte QUE `/me/library` (les anciennes routes
/// répondent 403, comme le vrai depuis février 2026) et note chaque appel.
async fn service(journal: Journal) -> SpotifyService {
    let noter = |methode: &'static str, journal: Journal| {
        move |uri: axum::http::Uri| {
            let journal = journal.clone();
            async move {
                journal
                    .lock()
                    .unwrap()
                    .push((methode.to_string(), uri.to_string()));
                if uri.path() == "/v1/me/library" {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::FORBIDDEN
                }
            }
        }
    };
    let app = axum::Router::new().fallback(
        axum::routing::put(noter("PUT", journal.clone())).delete(noter("DELETE", journal)),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    svc
}

#[tokio::test]
async fn spotify_2026_favoris_par_me_library_avec_des_uri() {
    let journal: Journal = Arc::default();
    let mut svc = service(journal.clone()).await;
    for (genre, id) in [
        ("tracks", "4uLU6hMCjMI75M1A2tKUQC"),
        ("albums", "spotify:album:4aawyAB9vmqN3uQ7FjRGTy"),
        ("artists", "0OdUWJ0sBjDrqHygGUXeCF"),
    ] {
        svc.add_favorite(genre, id)
            .await
            .unwrap_or_else(|e| panic!("ajout {genre} refusé : {e}"));
        svc.remove_favorite(genre, id)
            .await
            .unwrap_or_else(|e| panic!("retrait {genre} refusé : {e}"));
    }
    let appels = journal.lock().unwrap().clone();
    assert_eq!(
        appels,
        vec![
            (
                "PUT".into(),
                "/v1/me/library?uris=spotify%3Atrack%3A4uLU6hMCjMI75M1A2tKUQC".into()
            ),
            (
                "DELETE".into(),
                "/v1/me/library?uris=spotify%3Atrack%3A4uLU6hMCjMI75M1A2tKUQC".into()
            ),
            (
                "PUT".into(),
                "/v1/me/library?uris=spotify%3Aalbum%3A4aawyAB9vmqN3uQ7FjRGTy".into()
            ),
            (
                "DELETE".into(),
                "/v1/me/library?uris=spotify%3Aalbum%3A4aawyAB9vmqN3uQ7FjRGTy".into()
            ),
            (
                "PUT".into(),
                "/v1/me/library?uris=spotify%3Aartist%3A0OdUWJ0sBjDrqHygGUXeCF".into()
            ),
            (
                "DELETE".into(),
                "/v1/me/library?uris=spotify%3Aartist%3A0OdUWJ0sBjDrqHygGUXeCF".into()
            ),
        ]
    );
}

/// Les portées demandées couvrent l'écriture de la bibliothèque et le suivi
/// d'artistes ; sans elles, `/me/library` répond 403 (portée insuffisante).
#[test]
fn spotify_2026_les_portees_couvrent_les_favoris() {
    for portee in [
        "user-library-modify",
        "user-follow-read",
        "user-follow-modify",
    ] {
        assert!(
            SCOPES.split(' ').any(|p| p == portee),
            "portée {portee} absente de {SCOPES}"
        );
    }
}

/// Contre-épreuve : un type inconnu reste refusé, sans appel.
#[tokio::test]
async fn spotify_2026_type_de_favori_inconnu_refuse() {
    let journal: Journal = Arc::default();
    let mut svc = service(journal.clone()).await;
    assert!(svc.add_favorite("playlists-x", "abc").await.is_err());
    assert!(journal.lock().unwrap().is_empty());
}
