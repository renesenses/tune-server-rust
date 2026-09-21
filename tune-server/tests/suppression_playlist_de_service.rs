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

// ---------------------------------------------------------------------------
// La fusion, « au même endroit »
// ---------------------------------------------------------------------------

/// Bertrand, 21/09 : « Cela merge en local : erreur !! » — huit playlists
/// Qobuz cochées, et la fusion créait une playlist LOCALE. Vide.
///
/// Deux défauts qui s'additionnaient :
///
/// 1. `MergeRequest` ne déclarait pas `target_service`. Le client l'envoyait
///    depuis le début ; serde jette en silence un champ non déclaré, donc la
///    cible était TOUJOURS la bibliothèque.
/// 2. La boucle des sources « sautait pour le moment » toute source de
///    service. La playlist créée n'avait donc aucune piste — et la route
///    répondait `200`.
///
/// Un succès creux est pire qu'un refus : c'est lui qui a fait dire « la
/// fusion ne marche pas ».
#[tokio::test]
async fn fusionner_des_playlists_de_service_dans_le_local_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/playlist-manager/merge")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "playlists": [
                    { "service": "qobuz", "playlist_id": "1" },
                    { "service": "qobuz", "playlist_id": "2" }
                ],
                "target_name": "Fusion",
                "deduplicate": true
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "une source de service ne peut pas être fusionnée en local sans appariement : {corps}"
    );
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or_default()
            .contains("qobuz"),
        "{corps}"
    );
}

/// `target_service` doit être LU. S'il retombait dans l'oubli de serde, la
/// fusion repartirait dans la bibliothèque sans que rien ne le signale.
#[tokio::test]
async fn la_cible_de_fusion_est_lue_et_non_jetee_par_serde() {
    let state = etat();
    let app = appli(&state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/playlist-manager/merge")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "playlists": [
                    { "service": "qobuz", "playlist_id": "1" },
                    { "service": "qobuz", "playlist_id": "2" }
                ],
                "target_name": "Fusion",
                "target_service": "qobuz",
                "deduplicate": true
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    // Sans compte Qobuz, l'essai s'arrête au refus d'écriture — mais il
    // s'arrête CHEZ QOBUZ, et c'est tout ce qu'on veut prouver ici : la cible
    // n'est plus la bibliothèque.
    assert_ne!(
        statut,
        StatusCode::OK,
        "une playlist locale a encore été créée : {corps}"
    );
    let motif = corps["error"].as_str().unwrap_or_default();
    assert!(
        motif.contains("qobuz") || motif.contains("Qobuz"),
        "le refus doit venir de la cible, pas du local : {corps}"
    );
}

/// 🔴 « Je ne vois pas la playlist résultant du merge ! »
///
/// Elle EXISTAIT chez Qobuz — le journal du .18 le disait
/// (`playlists_merged_on_service service=qobuz playlist=70557265 ajoutees=3`)
/// — mais `GET /streaming/{service}/playlists` sert une liste **mémorisée
/// 2 minutes**. Les routes d'écriture de `tune-streaming-http` l'oublient
/// après chaque écriture ; la fusion et la suppression vivent ailleurs et
/// appellent le service directement : elles n'oubliaient rien.
///
/// Ce témoin ne rejoue pas la fusion (elle demande un compte) : il garde le
/// point exact qui manquait — l'oubli est ATTEIGNABLE depuis ce module, donc
/// il peut être appelé. Sans `pub`, ce fichier ne compilerait pas.
#[test]
fn l_oubli_de_la_liste_memorisee_est_atteignable_depuis_le_gestionnaire() {
    // Aucun service nommé « essai-oubli-fusion » n'existe : l'appel est sans
    // effet, et c'est sa seule JOIGNABILITÉ qui est en jeu.
    tune_streaming_http::purge_contenu_utilisateur("essai-oubli-fusion");
}
