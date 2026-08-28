//! Le relais support expose « marquer lu » (#2559).
//!
//! Le client web adressait `POST https://mozaiklabs.fr/api/v1/support/tickets/{id}/read`
//! **depuis le navigateur**, la clé de licence dans le corps. Trois testeurs
//! (Chrome/macOS `localhost:8888`, Firefox/Windows, la `.18` en `192.168.1.18:8888`)
//! ont produit le même blocage CORS : l'origine de la page n'est jamais
//! `https://mozaiklabs.fr`, et `localhost` n'échappe pas à la règle. Le suivi
//! des tickets est passé par le relais local ; ce dernier appel y manquait,
//! faute de route côté serveur.
//!
//! **Hermétique** : aucun appel réel à mozaiklabs.fr. Sans `license_key` ni
//! `mozaik_access_token` en réglages, `auth()` refuse la requête par 412 AVANT
//! toute sortie réseau — ce 412 est donc aussi la preuve qu'on n'est pas sorti.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use tune_server::state::AppState;

const READ: &str = "/api/v1/support/tickets/7/read";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Statut ET corps : le corps distingue « la route existe, l'auth manque » de
/// « la route n'existe pas ».
async fn appel(methode: &str, chemin: &str) -> (StatusCode, String) {
    let state = new_state();
    let app: Router = tune_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let corps = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    (status, corps)
}

/// Le cœur du correctif : la route est montée. Avant, ce chemin rendait 404 et
/// le client n'avait d'autre choix que d'appeler mozaiklabs en direct.
#[tokio::test]
async fn marquer_lu_est_servi_par_le_relais_local() {
    let (status, corps) = appel("POST", READ).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "attendu 412 (route montée, auth absente) — obtenu {status} : {corps}"
    );
    assert!(
        corps.contains("not_connected"),
        "le 412 doit venir du garde d'auth du relais, pas d'ailleurs : {corps}"
    );
}

/// `POST` seulement, comme `…/reply` : une lecture ne se marque pas par `GET`.
#[tokio::test]
async fn marquer_lu_refuse_les_autres_methodes() {
    let (status, corps) = appel("GET", READ).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "un GET sur {READ} doit rendre 405, pas {status} : {corps}"
    );
}

/// L'identifiant est un entier. Un 400 ici prouve que le chemin est bien celui
/// du relais — un 404 signifierait qu'on n'a rien monté du tout.
#[tokio::test]
async fn marquer_lu_rejette_un_identifiant_non_numerique() {
    let (status, corps) = appel("POST", "/api/v1/support/tickets/pas-un-nombre/read").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "identifiant non numérique : 400 attendu, obtenu {status} : {corps}"
    );
}

/// Les trois routes déjà relayées ne bougent pas. Sans elles, un `.route()`
/// mal placé pourrait faire passer le test précédent en cassant les voisines.
#[tokio::test]
async fn les_routes_deja_relayees_repondent_toujours() {
    for (methode, chemin) in [
        ("GET", "/api/v1/support/tickets"),
        ("GET", "/api/v1/support/tickets/7"),
        ("POST", "/api/v1/support/tickets/7/reply"),
    ] {
        let (status, corps) = appel(methode, chemin).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{methode} {chemin} n'est plus servi par le relais : {corps}"
        );
    }
}

/// Contre-preuve du montage : un chemin voisin qui n'existe pas rend bien 404.
/// Sans lui, les assertions ci-dessus pourraient passer sur un routeur qui
/// répond 412 à tout.
#[tokio::test]
async fn un_chemin_support_inexistant_rend_toujours_404() {
    let (status, _) = appel("POST", "/api/v1/support/tickets/7/unread").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
