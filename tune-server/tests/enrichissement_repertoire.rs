//! Contrat des DEUX routes d'enrichissement avec portée par répertoire (#1660).
//!
//! `POST /system/enrichment/run` — le pipeline canonique — et
//! `POST /library/enrich-all` — celle que le bouton « Enrichir les
//! métadonnées » de `SettingsView.svelte` appelle réellement
//! (`startBatchEnrich`). Borner la première sans la seconde laissait le geste
//! de l'utilisateur sur la passe non bornée : le mécanisme existait, la route
//! ne l'offrait pas.
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

// ---------------------------------------------------------------------------
// POST /library/enrich-all — LA route du bouton « Enrichir les métadonnées »
// ---------------------------------------------------------------------------

async fn post_enrich_all(app: &axum::Router, body: Option<&str>) -> (StatusCode, Value) {
    let req = match body {
        Some(b) => Request::post("/api/v1/library/enrich-all")
            .header("Content-Type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => Request::post("/api/v1/library/enrich-all")
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

/// Bibliothèque à deux répertoires dont AUCUNE piste n'est candidate : chaque
/// champ que la sélection de `/library/enrich-all` teste est déjà rempli, et
/// les artistes portent leur MBID. La passe acceptée ne trouve donc rien à
/// demander à MusicBrainz — le test reste hermétique — mais la PORTÉE, elle,
/// se calcule sur toutes les pistes locales et reste observable.
fn peupler_sans_candidat_reseau(state: &tune_server::state::AppState) {
    settings(state)
        .set("music_dirs", r#"["/music-i1660"]"#)
        .unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Miles Davis', 'mbid-1'); \
             INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'Kraftwerk', 'mbid-2'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (1, 'Kind of Blue', 1, 'c1'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (2, 'Autobahn', 2, 'c2'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, \
                                 musicbrainz_recording_id, genre, year, label, composer) \
               VALUES (10, 'So What', 1, 1, '/music-i1660/jazz/Kind of Blue/01.flac', 'local', \
                       'rec-10', 'Jazz', 1959, 'Columbia', 'Miles Davis'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, \
                                 musicbrainz_recording_id, genre, year, label, composer) \
               VALUES (11, 'Autobahn', 2, 2, '/music-i1660/electro/Autobahn/01.flac', 'local', \
                       'rec-11', 'Electronic', 1974, 'Philips', 'Hutter'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, \
                                 musicbrainz_recording_id, genre, year, label, composer) \
               VALUES (12, 'Voisin', 2, 2, '/music-i1660/jazz2/x/01.flac', 'local', \
                       'rec-12', 'Jazz', 1980, 'Blue Note', 'X');",
        )
        .unwrap();
}

/// TÉMOIN ANTI-RÉGRESSION. Sans corps, `/library/enrich-all` garde son contrat
/// mot pour mot : 202, `status: accepted`, `task_id`, et `directory` null —
/// c'est-à-dire la bibliothèque entière. Le client d'aujourd'hui, qui n'envoie
/// aucun corps, ne voit aucune différence.
#[tokio::test]
async fn enrich_all_sans_corps_reste_la_passe_complete() {
    let (app, state) = app_et_etat();
    peupler_sans_candidat_reseau(&state);
    let (status, body) = post_enrich_all(&app, None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"].as_str(), Some("accepted"));
    assert!(
        body["task_id"].as_str().is_some_and(|s| !s.is_empty()),
        "le task_id historique reste rendu"
    );
    assert!(
        body["directory"].is_null(),
        "sans path : toute la bibliothèque"
    );
}

/// Le geste de jfpaquet, sur la route qu'il actionne vraiment : un répertoire,
/// et la portée annoncée n'y compte QUE ce qui vit dessous. `/music-i1660/jazz2`
/// — le voisin au nom proche — n'entre pas, et l'artiste d'Electro non plus.
#[tokio::test]
async fn enrich_all_avec_path_borne_la_passe_au_repertoire() {
    let (app, state) = app_et_etat();
    peupler_sans_candidat_reseau(&state);
    let (status, body) = post_enrich_all(&app, Some(r#"{"path": "/music-i1660/jazz/"}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["directory"].as_str(), Some("/music-i1660/jazz"));
    assert_eq!(
        body["directory_tracks"].as_i64(),
        Some(1),
        "/music-i1660/jazz2 est un voisin, pas un sous-dossier"
    );
    assert_eq!(body["directory_albums"].as_i64(), Some(1));
    assert_eq!(
        body["directory_artists"].as_i64(),
        Some(1),
        "Kraftwerk (Electro et jazz2) hors portée"
    );
}

/// Un chemin hors des racines musicales est REFUSÉ, et le refus précède le
/// gate de quota : un chemin fautif ne coûte pas une passe de la journée. Sans
/// ce refus la route retomberait sur la bibliothèque entière — précisément ce
/// que l'utilisateur demandait d'éviter.
#[tokio::test]
async fn enrich_all_chemin_hors_racines_rejete_sans_consommer_le_quota() {
    let (app, state) = app_et_etat();
    peupler_sans_candidat_reseau(&state);
    let (status, body) = post_enrich_all(&app, Some(r#"{"path": "/ailleurs/jazz"}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str(), Some("path_outside_music_dirs"));
    assert_eq!(
        settings(&state).get("enrichment_daily_count").unwrap(),
        None,
        "le gate de quota ne doit pas avoir été touché"
    );
}

/// Toute composante `..` est refusée, ici comme sur le pipeline canonique :
/// une seule fonction valide les deux routes.
#[tokio::test]
async fn enrich_all_composante_parente_rejetee() {
    let (app, state) = app_et_etat();
    peupler_sans_candidat_reseau(&state);
    let (status, body) = post_enrich_all(&app, Some(r#"{"path": "/music-i1660/../etc"}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str(), Some("invalid_path"));
}

/// Un `path` vide dans un corps présent vaut passe complète, pas une erreur —
/// même règle que `/system/enrichment/run`, pour que le client n'ait pas à
/// distinguer « champ absent » de « champ vidé ».
#[tokio::test]
async fn enrich_all_path_vide_vaut_passe_complete() {
    let (app, state) = app_et_etat();
    peupler_sans_candidat_reseau(&state);
    let (status, body) = post_enrich_all(&app, Some(r#"{"path": "   "}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body["directory"].is_null());
}
