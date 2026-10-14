//! Liens et sauvegardes de `/playlist-manager` : des alias dépréciés.
//!
//! Ces routes font double emploi avec le greffon « Playlists converter »
//! (`/liens`, `/snapshots`). Les clients livrés passent par le greffon ; les
//! routes restent pour les anciens clients jusqu'à la 1.1, et le disent par
//! les en-têtes `Deprecation` (RFC 9745), `Sunset` (RFC 8594) et un `Link`
//! vers leur remplaçante.
//!
//! Ce fichier prouve trois choses :
//!
//! 1. chaque alias répond ENCORE comme avant (statut et effet en base) ;
//! 2. chaque réponse d'alias porte `Deprecation`, `Sunset` et le bon `Link` — y compris
//!    un 404 ;
//! 3. les routes de `/playlist-manager` qui ne sont PAS des doublons
//!    (`services`, `history`, `merge`…) ne portent pas l'en-tête : un marquage
//!    posé sur tout le préfixe passerait les deux premiers points.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const DEPRECATION: &str = "@1791331200";
/// Retrait annoncé pour la 1.1 (décision du 08/10/2026), RFC 8594.
const SUNSET: &str = "Fri, 01 Jan 2027 00:00:00 GMT";
const LIEN_LIENS: &str = "</api/v1/plugins/playlists-converter/liens>; rel=\"successor-version\"";
const LIEN_SAUVEGARDES: &str =
    "</api/v1/plugins/playlists-converter/snapshots>; rel=\"successor-version\"";

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    corps: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", "1");
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let en_tetes = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, en_tetes, json)
}

fn assert_deprecie(route: &str, en_tetes: &HeaderMap, lien: &str) {
    assert_eq!(
        en_tetes.get("deprecation").and_then(|v| v.to_str().ok()),
        Some(DEPRECATION),
        "{route} doit porter l'en-tête Deprecation ; en-têtes reçus : {en_tetes:?}"
    );
    assert_eq!(
        en_tetes.get("sunset").and_then(|v| v.to_str().ok()),
        Some(SUNSET),
        "{route} doit annoncer son retrait (en-tête Sunset) ; en-têtes reçus : {en_tetes:?}"
    );
    assert_eq!(
        en_tetes.get("link").and_then(|v| v.to_str().ok()),
        Some(lien),
        "{route} doit désigner sa remplaçante"
    );
}

fn reglage_json(state: &tune_server::state::AppState, cle: &str) -> Vec<Value> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(cle)
        .expect("lecture réglage")
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

async fn playlist_locale(app: &axum::Router, nom: &str) -> i64 {
    let (st, _, body) = appel(app, "POST", "/api/v1/playlists", Some(json!({"name": nom}))).await;
    assert_eq!(st, StatusCode::CREATED);
    body["id"].as_i64().expect("id playlist")
}

#[tokio::test]
async fn les_alias_des_liens_repondent_encore_et_se_disent_deprecies() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let pl = playlist_locale(&app, "Liee").await;

    let (st, h, _) = appel(&app, "GET", "/api/v1/playlist-manager/links", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_deprecie("GET /links", &h, LIEN_LIENS);

    let (st, h, lien) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/links",
        Some(json!({
            "local_playlist_id": pl,
            "service": "qobuz",
            "service_playlist_id": "123",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création : {lien}");
    assert_deprecie("POST /links", &h, LIEN_LIENS);
    assert_eq!(
        reglage_json(&state, "playlist_links").len(),
        1,
        "le lien est inscrit"
    );
    let id = lien["id"].as_i64().unwrap();

    // Synchro : le service n'est pas connecté, il ne rend aucune piste ; la
    // réponse d'avant arrive quand même, et la date de synchro est écrite.
    let (st, h, synchro) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlist-manager/links/{id}/sync"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "synchro : {synchro}");
    assert_deprecie("POST /links/{id}/sync", &h, LIEN_LIENS);
    assert_eq!(synchro["link_id"], id);
    assert!(
        !reglage_json(&state, "playlist_links")[0]["last_synced_at"].is_null(),
        "la synchro écrit sa date, comme avant"
    );

    let (st, h, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlist-manager/links/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_deprecie("DELETE /links/{id}", &h, LIEN_LIENS);
    assert!(
        reglage_json(&state, "playlist_links").is_empty(),
        "le lien est retiré"
    );

    // Même un 404 dit qu'il vient d'une route dépréciée.
    let (st, h, _) = appel(&app, "DELETE", "/api/v1/playlist-manager/links/999", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_deprecie("DELETE /links/999", &h, LIEN_LIENS);
}

#[tokio::test]
async fn les_alias_des_sauvegardes_repondent_encore_et_se_disent_deprecies() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    playlist_locale(&app, "A garder").await;

    let (st, h, corps) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/backup",
        Some(json!({"services": ["local"]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_deprecie("POST /backup", &h, LIEN_SAUVEGARDES);
    assert_eq!(corps["playlists_backed_up"], 1);

    let (st, h, liste) = appel(&app, "GET", "/api/v1/playlist-manager/backups", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_deprecie("GET /backups", &h, LIEN_SAUVEGARDES);
    let id = liste[0]["id"].as_i64().expect("une sauvegarde");

    let (st, h, _) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlist-manager/backups/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_deprecie("GET /backups/{id}", &h, LIEN_SAUVEGARDES);

    let (st, h, restaure) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlist-manager/backups/{id}/restore"),
        Some(json!({"target_name": "Restauree"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{restaure}");
    assert_deprecie("POST /backups/{id}/restore", &h, LIEN_SAUVEGARDES);
    assert_eq!(restaure["name"], "Restauree");

    let (st, h, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlist-manager/backups/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_deprecie("DELETE /backups/{id}", &h, LIEN_SAUVEGARDES);
    assert!(reglage_json(&state, "playlist_snapshots").is_empty());
}

#[tokio::test]
async fn les_routes_qui_ne_sont_pas_des_doublons_ne_sont_pas_depreciees() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    for (methode, route) in [
        ("GET", "/api/v1/playlist-manager/services"),
        ("GET", "/api/v1/playlist-manager/history"),
        ("GET", "/api/v1/playlist-manager/history/999"),
    ] {
        let (_, h, _) = appel(&app, methode, route, None).await;
        assert!(
            h.get("deprecation").is_none() && h.get("sunset").is_none(),
            "{methode} {route} n'est pas un doublon et ne doit pas se dire dépréciée"
        );
    }
}
