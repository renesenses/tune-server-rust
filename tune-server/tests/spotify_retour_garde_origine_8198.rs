//! renesenses/tune-server-rust#8198, volet 3 — le relais du site renvoie le
//! retour Spotify en formulaire `POST` depuis une autre origine. Éprouvé par
//! le VRAI routeur, contrôle d'origine compris (`garde_origine`) : ce seul
//! chemin, en `POST`, atteint son gestionnaire (le `state` et PKCE font foi) ;
//! tout le reste garde le refus des requêtes inter-sites.
//!
//! Les valeurs sont factices.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use tune_server::state::AppState;

const HOTE: &str = "192.168.1.18:8888";
const SITE: &str = "https://mozaiklabs.fr";

async fn appel(methode: Method, chemin: &str, origine: &str, corps: &str) -> (StatusCode, String) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app: Router = tune_server::routes::router(state);
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("host", HOTE)
        .header("origin", origine)
        .header("sec-fetch-site", "cross-site")
        .header("accept-language", "fr")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(corps.to_owned()))
        .unwrap();
    let rep = app.oneshot(req).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

#[tokio::test]
async fn le_retour_post_du_relais_atteint_son_gestionnaire() {
    let (statut, texte) = appel(
        Method::POST,
        "/api/v1/streaming/spotify/retour",
        SITE,
        "code=code-essai&state=state-inconnu-de-ce-serveur",
    )
    .await;
    assert_ne!(statut, StatusCode::FORBIDDEN, "{texte}");
    assert!(!texte.contains("cross_site_request"), "{texte}");
    // Le gestionnaire a répondu (page de retour), et le `state` inconnu y
    // est refusé.
    assert!(texte.contains("data-spotify-retour="), "{texte}");
    assert!(
        !texte.contains("data-spotify-retour=\"connecte\""),
        "{texte}"
    );
}

#[tokio::test]
async fn le_reste_garde_le_refus_inter_sites() {
    for (methode, chemin) in [
        (Method::POST, "/api/v1/streaming/spotify/auth"),
        (Method::PUT, "/api/v1/streaming/spotify/relais"),
        (Method::PUT, "/api/v1/streaming/spotify/retour"),
        (Method::POST, "/api/v1/streaming/spotify/retour/"),
        (Method::POST, "/api/v1/streaming/spotify/retourx"),
        (Method::PATCH, "/api/v1/system/config"),
    ] {
        let (statut, texte) = appel(methode.clone(), chemin, SITE, "code=x&state=y").await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "{methode} {chemin} : {texte}"
        );
        assert!(
            texte.contains("cross_site_request"),
            "{methode} {chemin} : {texte}"
        );
    }
}
