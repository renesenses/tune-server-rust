//! Supprimer une playlist CHEZ le service de streaming.
//!
//! Bertrand, en essayant l'écran Playlists refait : « pas de bouton pour
//! supprimer une playlist Tidal ! ». La carte d'une playlist locale porte une
//! corbeille depuis toujours ; celle d'une playlist de service n'en avait
//! aucune, et pour une raison qui n'était pas côté interface :
//!
//! - `StreamingService::delete_playlist` existait dans le trait avec une
//!   implémentation par défaut qui rend `Unsupported` ;
//! - **seul Qobuz** la redéfinissait ;
//! - et **aucune route HTTP ne l'appelait** — « écrit mais pas branché ».
//!
//! Poser le bouton ne suffisait donc pas : le clic n'avait nulle part où
//! aller. Ce témoin garde la route, et surtout le refus PRÉALABLE : un
//! service qui ne sait pas supprimer doit le dire avant qu'on lui parle,
//! sinon l'écran offre un bouton qui rendra une erreur opaque au clic.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

async fn supprime(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(chemin)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.expect("route absente");
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

async fn lis(app: &axum::Router, chemin: &str) -> Value {
    let req = Request::builder().uri(chemin).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.expect("route absente");
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap_or(Value::Null)
}

/// La route existe. Sans ce témoin, tout le reste passerait par un 404 que
/// `oneshot` rend sans broncher.
#[tokio::test]
async fn la_route_existe_et_ne_rend_pas_404() {
    let state = etat();
    let app = appli(&state);
    let (statut, _) = supprime(&app, "/api/v1/playlist-manager/playlists/tidal/uuid-x").await;
    assert_ne!(
        statut,
        StatusCode::NOT_FOUND,
        "la route DELETE /playlist-manager/playlists/{{service}}/{{id}} n'est pas montée"
    );
}

/// Un service connu mais sans compte n'annonce pas la capacité : on refuse
/// AVANT de lui parler. C'est ce 501 que l'interface évite en ne posant pas
/// le bouton.
#[tokio::test]
async fn un_service_sans_compte_refuse_avant_tout_appel() {
    let state = etat();
    let app = appli(&state);
    let (statut, corps) = supprime(&app, "/api/v1/playlist-manager/playlists/tidal/uuid-x").await;
    assert_eq!(statut, StatusCode::NOT_IMPLEMENTED, "corps : {corps}");
}

/// Une playlist locale se supprime par `DELETE /playlists/{id}`. Laisser
/// passer « local » ici aurait rendu 400 « unknown service » — un motif faux.
#[tokio::test]
async fn local_est_renvoye_vers_sa_propre_route() {
    let state = etat();
    let app = appli(&state);
    let (statut, corps) = supprime(&app, "/api/v1/playlist-manager/playlists/local/7").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or_default()
            .contains("/playlists/"),
        "le motif doit nommer la bonne route : {corps}"
    );
}

#[tokio::test]
async fn un_service_inconnu_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (statut, corps) = supprime(&app, "/api/v1/playlist-manager/playlists/napster/42").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or_default()
            .contains("napster"),
        "{corps}"
    );
}

/// L'écran décide de poser ou non la corbeille d'après `/services`. Le champ
/// doit donc y être — et `local`, qui sait supprimer, le porter à `true`.
#[tokio::test]
async fn services_annonce_la_capacite_de_suppression() {
    let state = etat();
    let app = appli(&state);
    let corps = lis(&app, "/api/v1/playlist-manager/services").await;
    assert_eq!(
        corps["local"]["supports_delete"],
        Value::Bool(true),
        "{corps}"
    );
    for (nom, svc) in corps.as_object().expect("objet de services") {
        assert!(
            svc.get("supports_delete").is_some(),
            "{nom} n'annonce pas supports_delete : {svc}"
        );
    }
}
