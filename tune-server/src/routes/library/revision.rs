//! La révision de la bibliothèque côté API — #7619, volet L5.
//!
//! - `GET /api/v1/library/revision` rend `{"revision": "<démarrage>-<n>"}`,
//!   jamais mis en cache : c'est la question que le client pose pour savoir si
//!   les pages qu'il garde dans IndexedDB valent encore ;
//! - la couche [`suivre_les_modifications`] avance la révision après toute
//!   requête qui modifie (`POST`, `PUT`, `PATCH`, `DELETE`) et réussit, et
//!   pose l'en-tête `X-Library-Revision` sur chaque réponse.
//!
//! Le compteur lui-même est dans `tune_core::library::revision`.

use axum::Json;
use axum::http::{HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};

/// En-tête porté par chaque réponse de `/library` et `/metadata`.
pub(crate) const EN_TETE: &str = "x-library-revision";

pub(super) async fn revision_courante() -> Response {
    let mut reponse = Json(serde_json::json!({
        "revision": tune_core::library::revision::courante(),
    }))
    .into_response();
    reponse
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reponse
}

/// Une méthode qui peut modifier la bibliothèque.
fn modifie(methode: &Method) -> bool {
    matches!(
        *methode,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Couche du routeur : avance la révision après une modification réussie, et
/// annonce la révision sur chaque réponse.
///
/// Après, et non avant : une modification refusée (400, 404, 403) ne change
/// rien. Un `POST` de lecture (il en existe, recherche par corps) avance la
/// révision pour rien : un rechargement de trop, jamais une page fausse.
///
/// Une tâche lancée par un `POST` (scan, enrichissement) finit plus tard : la
/// révision avancée ici précède ses écritures. Ce sont alors ses événements
/// `library.*` sur le bus qui l'avancent à nouveau, au fil du travail.
pub(crate) async fn suivre_les_modifications(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let ecriture = modifie(request.method());
    let mut reponse = next.run(request).await;
    if ecriture && reponse.status().is_success() {
        tune_core::library::revision::avancer();
    }
    if let Ok(v) = HeaderValue::from_str(&tune_core::library::revision::courante()) {
        reponse.headers_mut().insert(EN_TETE, v);
    }
    reponse
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    fn routeur() -> Router {
        Router::new()
            .route("/revision", get(revision_courante))
            .route("/ok", post(|| async { StatusCode::OK }))
            .route("/refus", post(|| async { StatusCode::BAD_REQUEST }))
            .route("/lecture", get(|| async { "x" }))
            .layer(axum::middleware::from_fn(suivre_les_modifications))
    }

    async fn revision(app: &Router) -> String {
        let r = app
            .clone()
            .oneshot(Request::get("/revision").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        let corps = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&corps).unwrap();
        v["revision"].as_str().unwrap().to_string()
    }

    async fn appel(app: &Router, m: Method, uri: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(m)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Témoin : une modification réussie change la révision rendue.
    ///
    /// Le compteur est global au processus et d'autres tests l'avancent en
    /// parallèle : on ne vérifie que des inégalités qui tiennent quoi qu'ils
    /// fassent (un `POST` réussi avance AU MOINS d'un cran).
    #[tokio::test]
    async fn une_modification_reussie_avance_la_revision() {
        let app = routeur();
        let avant = revision(&app).await;
        let r = appel(&app, Method::POST, "/ok").await;
        assert_eq!(r.status(), StatusCode::OK);
        let annoncee = r.headers()[EN_TETE].to_str().unwrap().to_string();
        assert_ne!(
            avant, annoncee,
            "un POST réussi sous /library doit avancer la révision"
        );
        let n = |s: &str| s.rsplit('-').next().unwrap().parse::<u64>().unwrap();
        assert!(n(&annoncee) > n(&avant));
    }

    #[tokio::test]
    async fn chaque_reponse_porte_la_revision() {
        let app = routeur();
        let r = appel(&app, Method::GET, "/lecture").await;
        assert!(r.headers().contains_key(EN_TETE));
        let r = appel(&app, Method::POST, "/refus").await;
        assert!(r.headers().contains_key(EN_TETE));
    }

    #[test]
    fn methodes_qui_modifient() {
        assert!(modifie(&Method::POST));
        assert!(modifie(&Method::DELETE));
        assert!(modifie(&Method::PUT));
        assert!(modifie(&Method::PATCH));
        assert!(!modifie(&Method::GET));
        assert!(!modifie(&Method::HEAD));
        assert!(!modifie(&Method::OPTIONS));
    }
}
