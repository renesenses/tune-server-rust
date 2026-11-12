//! Février 2026 — Spotify a retiré aux applications en mode développement
//! (celle de chaque utilisateur de Tune) `GET /browse/new-releases`,
//! `GET /browse/categories` et `/browse/categories/{id}/…` (guide de
//! migration de février 2026). Comme la sélection éditoriale (#8399) et les
//! titres phares (#8485) : un 403 donne une rubrique vide, sans erreur (l'écran affichait un
//! « HTTP 502 » brut), et Spotify n'est plus interrogé à chaque ouverture.
//!
//! Doublure locale de l'API : aucune requête ne part vers Spotify.

use super::*;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

/// Un faux Spotify dont les trois routes de navigation répondent `statut`.
async fn service(statut: u16, appels: Arc<AtomicUsize>) -> SpotifyService {
    let repondre = move || {
        let appels = appels.clone();
        async move {
            appels.fetch_add(1, Ordering::SeqCst);
            (
                axum::http::StatusCode::from_u16(statut).unwrap(),
                axum::Json(json!({"error": {"status": statut, "message": "Forbidden"}})),
            )
        }
    };
    let app = axum::Router::new()
        .route(
            "/v1/browse/new-releases",
            axum::routing::get(repondre.clone()),
        )
        .route(
            "/v1/browse/categories",
            axum::routing::get(repondre.clone()),
        )
        .route(
            "/v1/browse/categories/{id}/playlists",
            axum::routing::get(repondre),
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
async fn spotify_2026_nouveautes_et_genres_retires_vides_sans_erreur_ni_rappel() {
    {
        let statut = 403u16;
        let appels = Arc::new(AtomicUsize::new(0));
        let mut svc = service(statut, appels.clone()).await;
        for _ in 0..3 {
            assert!(
                svc.get_new_releases()
                    .await
                    .unwrap_or_else(|e| panic!("{statut} : nouveautés en erreur : {e}"))
                    .is_empty()
            );
            assert!(
                svc.get_genres(None)
                    .await
                    .unwrap_or_else(|e| panic!("{statut} : genres en erreur : {e}"))
                    .is_empty()
            );
            assert!(
                svc.get_genre_albums("0JQ5DAqbMKFQ00XGBls6ym", 10)
                    .await
                    .unwrap_or_else(|e| panic!("{statut} : albums du genre en erreur : {e}"))
                    .is_empty()
            );
        }
        assert_eq!(
            appels.load(Ordering::SeqCst),
            2,
            "{statut} : une requête par point d'accès retiré, puis mémorisé \
             (les albums d'un genre suivent le retrait des catégories)"
        );

        // Une autre application (Client ID) peut y avoir droit : redemandé.
        svc.set_client_id("client-autre");
        svc.access_token = Some("jeton".into());
        assert!(svc.get_new_releases().await.expect("vide").is_empty());
        assert_eq!(appels.load(Ordering::SeqCst), 3, "{statut} : redemandé");
    }
}

/// Contre-épreuve (#6825) : tout autre refus (404, 500) reste une erreur, et
/// n'est pas mémorisé.
#[tokio::test]
async fn spotify_2026_un_autre_refus_reste_une_erreur() {
    for statut in [404u16, 500] {
        let appels = Arc::new(AtomicUsize::new(0));
        let svc = service(statut, appels.clone()).await;
        assert!(svc.get_new_releases().await.is_err(), "{statut}");
        assert!(svc.get_new_releases().await.is_err(), "{statut}");
        assert!(svc.get_genres(None).await.is_err(), "{statut}");
        assert_eq!(appels.load(Ordering::SeqCst), 3, "{statut}");
    }
}

/// Le retrait se lit par `rubrique_retiree` (l'en-tête de la route), et
/// seulement après le refus : avant, rien n'est dit retiré.
#[tokio::test]
async fn spotify_2026_le_retrait_se_lit_par_rubrique_retiree() {
    use crate::streaming::traits::RubriqueRetirable as R;
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = service(403, appels).await;
    assert!(!svc.rubrique_retiree(R::Nouveautes));
    assert!(!svc.rubrique_retiree(R::Genres));
    svc.get_new_releases().await.expect("vide");
    assert!(svc.rubrique_retiree(R::Nouveautes));
    assert!(
        !svc.rubrique_retiree(R::Genres),
        "les genres n'ont pas été demandés"
    );
    svc.get_genres(None).await.expect("vide");
    assert!(svc.rubrique_retiree(R::Genres));
    assert!(!svc.rubrique_retiree(R::Selection));
}
