//! #9082 — un compte Spotify relié avant #8579 n'a pas la portée
//! `user-follow-read` : `/me/following` répond 403 « Insufficient client
//! scope ». Ce 403 devenait une erreur ordinaire, rendue en 502 par la route
//! des favoris, et l'écran Streaming affichait une panne à chaque lancement.
//!
//! Un faux Spotify local répond à `/v1/me/following` selon le scénario et
//! compte les appels.
use super::*;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Faux {
    appels: Arc<AtomicUsize>,
    statut: u16,
    message: &'static str,
}

async fn following(State(f): State<Faux>) -> (StatusCode, axum::Json<serde_json::Value>) {
    f.appels.fetch_add(1, Ordering::SeqCst);
    if f.statut == 200 {
        return (
            StatusCode::OK,
            axum::Json(json!({"artists": {
                "items": [{"id": "r1", "name": "artiste 1", "images": []}],
                "next": null, "total": 1, "limit": 50, "cursors": {"after": null}
            }})),
        );
    }
    (
        StatusCode::from_u16(f.statut).expect("statut"),
        axum::Json(json!({"error": {"status": f.statut, "message": f.message}})),
    )
}

async fn faux_spotify(
    statut: u16,
    message: &'static str,
    portees: Option<&str>,
) -> (SpotifyService, Arc<AtomicUsize>) {
    let appels = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/v1/me/following", get(following))
        .with_state(Faux {
            appels: appels.clone(),
            statut,
            message,
        });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse locale"));
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    svc.portees = portees.map(Into::into);
    (svc, appels)
}

fn code_du_refus(e: &TuneError) -> Option<&'static str> {
    match e {
        TuneError::Refused { code, .. } => Some(code),
        _ => None,
    }
}

/// Portées connues, sans `user-follow-read` : refus au code stable, sans
/// aucun appel à Spotify.
#[tokio::test]
async fn sans_la_portee_les_artistes_suivis_demandent_une_reconnexion_9082() {
    let (svc, appels) = faux_spotify(
        403,
        "Insufficient client scope",
        Some("user-read-private user-library-read streaming"),
    )
    .await;
    let e = svc.get_user_artists().await.expect_err("refus attendu");
    assert_eq!(
        code_du_refus(&e),
        Some(CODE_RECONNEXION_ARTISTES_SUIVIS),
        "#9082 : la portée manquante doit être un refus qualifié, pas une panne : {e}"
    );
    assert!(
        !e.to_string().contains("403"),
        "le motif ne doit pas relancer le rafraîchissement du jeton : {e}"
    );
    assert_eq!(appels.load(Ordering::SeqCst), 0, "aucun appel à Spotify");
}

/// Portées inconnues (liaison restaurée sans `scope`) : le 403 « Insufficient
/// client scope » de Spotify devient le même refus qualifié.
#[tokio::test]
async fn un_403_de_portee_devient_un_refus_qualifie_9082() {
    let (svc, appels) = faux_spotify(403, "Insufficient client scope", None).await;
    let e = svc.get_user_artists().await.expect_err("refus attendu");
    assert_eq!(
        code_du_refus(&e),
        Some(CODE_RECONNEXION_ARTISTES_SUIVIS),
        "#9082 : un 403 de portée sur /me/following doit être qualifié : {e}"
    );
    assert_eq!(appels.load(Ordering::SeqCst), 1);
}

/// Contre-épreuve : avec la portée accordée, un 403 qui ne parle pas de
/// portée reste l'erreur de Spotify.
#[tokio::test]
async fn avec_la_portee_un_autre_403_reste_une_erreur_9082() {
    let (svc, _) = faux_spotify(403, "Forbidden", Some("user-follow-read streaming")).await;
    let e = svc.get_user_artists().await.expect_err("erreur attendue");
    assert_eq!(
        code_du_refus(&e),
        None,
        "pas de reconnexion à proposer : {e}"
    );
    assert!(e.to_string().contains("HTTP 403"), "{e}");
}

/// Témoin : avec la portée, les artistes suivis remontent.
#[tokio::test]
async fn avec_la_portee_les_artistes_suivis_remontent_9082() {
    let (svc, appels) = faux_spotify(200, "", Some("user-follow-read")).await;
    let artistes = svc.get_user_artists().await.expect("artistes suivis");
    assert_eq!(artistes.len(), 1);
    assert_eq!(artistes[0].id, "r1");
    assert_eq!(appels.load(Ordering::SeqCst), 1);
}
