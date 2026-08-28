//! Contrat de `POST /library/albums/{id}/reidentify` (#2128).
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Les chemins testés ici sont
//! exactement ceux qui rendent une réponse *avant* la moindre requête réseau —
//! album inconnu, album sans piste. C'est délibéré, et ce n'est pas une demi-
//! mesure : ce sont les deux cas où la route pourrait détruire quelque chose
//! sans rien avoir à y gagner, donc les deux qui méritent un garde-fou. La
//! logique de base (effacement, restitution, bornes, correspondance des pistes)
//! est couverte par les tests unitaires de
//! `tune_core::metadata::reidentify`, qui ne touchent pas non plus au réseau.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn post(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn mbid_album(state: &tune_server::state::AppState, id: i64) -> Option<String> {
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .query_one(
            "SELECT musicbrainz_release_id FROM albums WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()
        .first()
        .and_then(|v| v.as_string())
}

/// La route est bien montée. Un 404 nu voudrait dire que le routeur ne la
/// connaît pas ; ici le corps doit porter l'explication du handler.
#[tokio::test]
async fn album_inconnu_rend_un_404_explique_et_non_un_404_de_routeur() {
    let (app, _state) = app_et_etat();

    let (status, body) = post(&app, "/api/v1/library/albums/999999/reidentify").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["error"].as_str(),
        Some("album introuvable"),
        "corps vide : la route n'est probablement pas montee"
    );
}

/// ⚠ Un album sans piste n'a rien à ré-identifier — et surtout, son
/// identification ne doit PAS être effacée au passage. C'est le cas où un
/// « efface puis relance » naïf laisserait l'album plus pauvre qu'il ne l'a
/// trouvé, sans contrepartie.
#[tokio::test]
async fn album_sans_piste_conserve_son_identification() {
    let (app, state) = app_et_etat();
    state
        .backend
        .execute_batch(
            "INSERT INTO albums (id, title, musicbrainz_release_id, musicbrainz_release_group_id) \
             VALUES (7, 'Album sans piste', 'rel-EXISTANT', 'rg-EXISTANT');",
        )
        .unwrap();

    let (status, body) = post(&app, "/api/v1/library/albums/7/reidentify").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["verdict"].as_str(), Some("no_tracks"));
    assert_eq!(body["tracks_total"].as_i64(), Some(0));
    assert_eq!(
        mbid_album(&state, 7).as_deref(),
        Some("rel-EXISTANT"),
        "l'identification a ete effacee alors qu'il n'y avait rien a ré-identifier"
    );
}

/// La route ne répond qu'au POST : un GET ne doit pas déclencher une écriture.
#[tokio::test]
async fn la_reidentification_n_est_pas_accessible_en_get() {
    let (app, _state) = app_et_etat();

    let resp = app
        .oneshot(
            Request::get("/api/v1/library/albums/7/reidentify")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
