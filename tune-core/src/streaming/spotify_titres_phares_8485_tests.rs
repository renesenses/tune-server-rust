//! #8485 — depuis février 2026, Spotify ne sert plus
//! `GET /artists/{id}/top-tracks` aux applications en mode développement
//! (403). Les titres phares sont une section vide, sans erreur, et Spotify
//! n'est plus interrogé à chaque fiche artiste.
//!
//! Doublure locale de l'API : aucune requête ne part vers Spotify.

use super::*;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

/// Un faux Spotify dont `/artists/{id}/top-tracks` répond `statut`.
async fn service(statut: u16, appels: Arc<AtomicUsize>) -> SpotifyService {
    let app = axum::Router::new().route(
        "/v1/artists/{id}/top-tracks",
        axum::routing::get(move || {
            let appels = appels.clone();
            async move {
                appels.fetch_add(1, Ordering::SeqCst);
                (
                    axum::http::StatusCode::from_u16(statut).unwrap(),
                    axum::Json(json!({"error": {"status": statut, "message": "Forbidden"}})),
                )
            }
        }),
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
async fn spotify_8485_titres_phares_refuses_en_403_vides_sans_erreur_ni_rappel() {
    let appels = Arc::new(AtomicUsize::new(0));
    let mut svc = service(403, appels.clone()).await;

    for artiste in ["0OdUWJ0sBjDrqHygGUXeCF", "4Z8W4fKeB5YxbusRsdQVPb", "x"] {
        let titres = svc
            .get_artist_top_tracks(artiste)
            .await
            .expect("#8485 : un 403 ne remonte plus en erreur");
        assert!(titres.is_empty());
    }
    assert_eq!(
        appels.load(Ordering::SeqCst),
        1,
        "un seul appel, puis mémorisé"
    );

    // Une autre application (Client ID) peut y avoir droit : redemandé.
    svc.set_client_id("client-ancien");
    svc.access_token = Some("jeton".into());
    assert!(
        svc.get_artist_top_tracks("x")
            .await
            .expect("403 → vide")
            .is_empty()
    );
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "redemandé après changement d'application"
    );
}

/// Un autre refus (ici 500) reste une erreur, et n'est pas mémorisé.
#[tokio::test]
async fn spotify_8485_un_autre_refus_reste_une_erreur() {
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = service(500, appels.clone()).await;
    assert!(svc.get_artist_top_tracks("x").await.is_err());
    assert!(svc.get_artist_top_tracks("x").await.is_err());
    assert_eq!(appels.load(Ordering::SeqCst), 2);
}
