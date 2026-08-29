//! Contrat de `POST /system/enrichment/run` avec portée par répertoire (#1660).
//!
//! **Hermétique : aucun appel réseau.** Les cas testés rendent leur réponse
//! avant toute requête sortante : chemin rejeté (la route refuse AVANT le gate
//! de quota et avant tout spawn), ou passe acceptée sur une bibliothèque dont
//! tous les candidats réseau sont déjà couverts. La sélection des candidats
//! par portée — pistes hors répertoire jamais candidates — est verrouillée par
//! les tests unitaires de `tune_core::metadata::enrich_scope`, hors réseau eux
//! aussi.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

fn settings(state: &tune_server::state::AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

async fn post_run(app: &axum::Router, body: Option<&str>) -> (StatusCode, Value) {
    let req = match body {
        Some(b) => Request::post("/api/v1/system/enrichment/run")
            .header("Content-Type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => Request::post("/api/v1/system/enrichment/run")
            .body(Body::empty())
            .unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Bibliothèque à deux répertoires dont TOUS les candidats réseau sont déjà
/// couverts (pochettes posées) : une passe acceptée n'a rien à télécharger.
fn peupler(state: &tune_server::state::AppState) {
    settings(state)
        .set("music_dirs", r#"["/music-p2a1"]"#)
        .unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Miles Davis', 'mbid-1'); \
             INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'Kraftwerk', 'mbid-2'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (1, 'Kind of Blue', 1, 'c1'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (2, 'Autobahn', 2, 'c2'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
               VALUES (10, 'So What', 1, 1, '/music-p2a1/Jazz/Kind of Blue/01.flac', 'local'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
               VALUES (11, 'Autobahn', 2, 2, '/music-p2a1/Electro/Autobahn/01.flac', 'local');",
        )
        .unwrap();
}

/// Un chemin hors des racines musicales est REFUSÉ — pas de repli silencieux
/// vers la passe complète (qui serait précisément ce que l'utilisateur voulait
/// éviter), et pas de quota consommé : le refus précède le gate.
#[tokio::test]
async fn chemin_hors_racines_rejete_sans_consommer_le_quota() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = post_run(&app, Some(r#"{"path": "/ailleurs/Jazz"}"#)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str(), Some("path_outside_music_dirs"));
    assert_eq!(
        settings(&state).get("enrichment_daily_count").unwrap(),
        None,
        "le gate de quota ne doit pas avoir été touché"
    );
    assert_eq!(
        settings(&state).get("enrichment_last_run").unwrap(),
        None,
        "un refus ne date aucune passe"
    );
}

/// Toute composante `..` est refusée : `/music-p2a1/../etc` « appartient » à
/// la racine au sens du préfixe, mais sort du périmètre une fois résolu.
#[tokio::test]
async fn composante_parente_rejetee() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = post_run(&app, Some(r#"{"path": "/music-p2a1/../etc"}"#)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str(), Some("invalid_path"));
}

/// Rétro-compatibilité : sans corps, contrat historique inchangé — 202, passe
/// complète (`directory` null) et horodatage de passe posé.
#[tokio::test]
async fn sans_corps_le_contrat_historique_est_inchange() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = post_run(&app, None).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"].as_str(), Some("enrichment_run_started"));
    assert!(body["directory"].is_null(), "sans path : passe complète");
    assert!(
        settings(&state)
            .get("enrichment_last_run")
            .unwrap()
            .is_some(),
        "la passe complète se date"
    );
}

/// Chemin valide : 202, la réponse annonce la portée calculée — et l'album
/// témoin de l'autre répertoire n'y figure pas. Une passe limitée ne se fait
/// pas passer pour une passe complète (`enrichment_last_run` intact).
#[tokio::test]
async fn chemin_valide_rend_la_portee_et_ne_se_date_pas() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = post_run(&app, Some(r#"{"path": "/music-p2a1/Jazz/"}"#)).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"].as_str(), Some("enrichment_run_started"));
    assert_eq!(body["directory"].as_str(), Some("/music-p2a1/Jazz"));
    assert_eq!(body["directory_tracks"].as_i64(), Some(1));
    assert_eq!(body["directory_albums"].as_i64(), Some(1));
    assert_eq!(
        body["directory_artists"].as_i64(),
        Some(1),
        "Kraftwerk (répertoire Electro) hors portée"
    );
    assert_eq!(
        settings(&state).get("enrichment_last_run").unwrap(),
        None,
        "une passe limitée ne date pas la passe complète"
    );
}

/// Un `path` vide ou absent dans un corps JSON présent = passe complète, pas
/// une erreur.
#[tokio::test]
async fn corps_sans_path_vaut_passe_complete() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = post_run(&app, Some(r#"{"path": "  "}"#)).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body["directory"].is_null());
}
