//! #7872 — la requête avancée d'une collection intelligente
//! (`smart_collections.expression`), sur SQLite. La même épreuve sur
//! PostgreSQL vit dans `tune-server/tests/pg_requete_avancee_7872.rs`.
//!
//! Banc : trois albums de Karajan, un de Böhm.
//!
//! | album          | année | genre     | compositeur | chef    | pistes |
//! |----------------|-------|-----------|-------------|---------|--------|
//! | Cinquième 1963 | 1963  | Classique | Beethoven   | Karajan | 2      |
//! | Septième live  | 1965  | Live      | Beethoven   | Karajan | 1      |
//! | Neuvième 1976  | 1976  | Classique | Beethoven   | Karajan | 1      |
//! | Mahler 1970    | 1970  | Classique | Mahler      | Böhm    | 1      |
//!
//! Plus un album favori de Qobuz : il entre dans une collection « Source »
//! sans expression, jamais dans une collection qui en porte une.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_http_types::ActiveProfile;

use crate::SmartHttpState;

const REQUETE_DE_L_ISSUE: &str = "composer:beethoven conductor:karajan year:1960..1969 -genre:live";

fn etat() -> SmartHttpState {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    // Une base neuve porte les collections du semis : on part d'une table
    // vide pour lire nos seules collections.
    db.execute_batch(
        "DELETE FROM smart_collections;
         INSERT INTO artists (id, name) VALUES (1, 'Berliner Philharmoniker');
         INSERT INTO albums (id, title, artist_id, year, source) VALUES
             (10, 'Cinquième 1963', 1, 1963, 'local'),
             (11, 'Septième live', 1, 1965, 'local'),
             (12, 'Neuvième 1976', 1, 1976, 'local'),
             (13, 'Mahler 1970', 1, 1970, 'local');
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, year, genre) VALUES
             (100, 'Allegro con brio', 10, 1, '/q7872/1.flac', 1963, 'Classique'),
             (101, 'Andante con moto', 10, 1, '/q7872/2.flac', 1963, 'Classique'),
             (102, 'Poco sostenuto', 11, 1, '/q7872/3.flac', 1965, 'Live'),
             (103, 'Ode', 12, 1, '/q7872/4.flac', 1976, 'Classique'),
             (104, 'Adagietto', 13, 1, '/q7872/5.flac', 1970, 'Classique');
         INSERT INTO track_credits (track_id, artist_name, role) VALUES
             (100, 'Ludwig van Beethoven', 'composer'), (100, 'Herbert von Karajan', 'conductor'),
             (101, 'Ludwig van Beethoven', 'composer'), (101, 'Herbert von Karajan', 'conductor'),
             (102, 'Ludwig van Beethoven', 'composer'), (102, 'Herbert von Karajan', 'conductor'),
             (103, 'Ludwig van Beethoven', 'composer'), (103, 'Herbert von Karajan', 'conductor'),
             (104, 'Gustav Mahler', 'composer'), (104, 'Karl Böhm', 'conductor');
         INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title, artist)
             VALUES (1, 'album', 'qobuz', 'q1', 'Qobuz 1963', 'Karajan');",
    )
    .expect("banc");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    SmartHttpState::new(backend)
}

async fn corps(reponse: axum::response::Response) -> (axum::http::StatusCode, Value) {
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn creer(etat: &SmartHttpState, corps_json: Value) -> (axum::http::StatusCode, Value) {
    let corps_json = serde_json::from_value(corps_json).expect("corps de création");
    let reponse = match super::create_collection(State(etat.clone()), Json(corps_json)).await {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    };
    corps(reponse).await
}

async fn albums(etat: &SmartHttpState, id: i64) -> BTreeSet<String> {
    let reponse = match super::resolve_albums(State(etat.clone()), ActiveProfile(1), Path(id)).await
    {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    };
    let (statut, v) = corps(reponse).await;
    assert!(statut.is_success(), "albums de {id} : {statut} {v}");
    v.as_array()
        .expect("tableau d'albums")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn noms(liste: &[&str]) -> BTreeSet<String> {
    liste.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn une_collection_applique_son_expression_a_chaque_ouverture_7872() {
    let etat = etat();
    let (statut, c) = creer(
        &etat,
        json!({"name": "Karajan années 60", "rules": [], "expression": REQUETE_DE_L_ISSUE}),
    )
    .await;
    assert_eq!(statut, axum::http::StatusCode::CREATED, "{c}");
    assert_eq!(
        c["expression"], REQUETE_DE_L_ISSUE,
        "relue telle quelle : {c}"
    );
    let id = c["id"].as_i64().unwrap();
    assert_eq!(albums(&etat, id).await, noms(&["Cinquième 1963"]));

    // Appliquée À CHAQUE ouverture : une piste qui gagne le crédit entre
    // dans la collection sans qu'on la réenregistre.
    etat.backend
        .execute("UPDATE tracks SET year = 1969 WHERE id = 103", &[])
        .unwrap();
    assert_eq!(
        albums(&etat, id).await,
        noms(&["Cinquième 1963", "Neuvième 1976"])
    );

    // Règles ET expression : la règle « année 1976 » garde la seule
    // Neuvième parmi les albums de l'expression.
    let (_, c2) = creer(
        &etat,
        json!({
            "name": "Beethoven 1976",
            "rules": [{"field": "year", "op": "=", "value": "1976"}],
            "expression": "composer:beethoven",
        }),
    )
    .await;
    assert_eq!(
        albums(&etat, c2["id"].as_i64().unwrap()).await,
        noms(&["Neuvième 1976"])
    );

    // La liste des collections compte avec la même expression.
    let Ok(Json(liste)) = super::list_collections(State(etat.clone()), ActiveProfile(1)).await
    else {
        panic!("liste");
    };
    let ligne = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .expect("la collection est listée")
        .clone();
    assert_eq!(ligne["album_count"], 2, "{ligne}");
    assert_eq!(ligne["track_count"], 3, "{ligne}");
    assert_eq!(ligne["expression"], REQUETE_DE_L_ISSUE);
}

#[tokio::test]
async fn les_albums_de_service_n_entrent_pas_sous_une_expression_7872() {
    let etat = etat();
    let source = json!([{"field": "source", "op": "=", "value": "qobuz"}]);
    let (_, sans) = creer(&etat, json!({"name": "Qobuz", "rules": source})).await;
    assert!(
        albums(&etat, sans["id"].as_i64().unwrap())
            .await
            .contains("Qobuz 1963"),
        "témoin : sans expression, le favori Qobuz entre"
    );
    let (_, avec) = creer(
        &etat,
        json!({"name": "Qobuz Karajan", "rules": source, "expression": "conductor:karajan"}),
    )
    .await;
    let vus = albums(&etat, avec["id"].as_i64().unwrap()).await;
    assert!(!vus.contains("Qobuz 1963"), "{vus:?}");
}

#[tokio::test]
async fn une_expression_mal_formee_est_refusee_avec_sa_position_7872() {
    let etat = etat();
    let (statut, refus) = creer(
        &etat,
        json!({"name": "Fautive", "rules": [], "expression": "year:1963 ratng:5"}),
    )
    .await;
    assert_eq!(statut, axum::http::StatusCode::BAD_REQUEST, "{refus}");
    assert_eq!(refus["position"], 10, "{refus}");
    assert!(refus["attendu"].as_str().is_some_and(|a| !a.is_empty()));
    assert!(refus["message"].as_str().is_some_and(|m| !m.is_empty()));
    let n = etat
        .backend
        .query_one("SELECT COUNT(*) FROM smart_collections", &[])
        .unwrap()
        .and_then(|l| l[0].as_i64());
    assert_eq!(n, Some(0), "rien n'est écrit");

    // Mise à jour : refusée de même, puis effacée par `null`.
    let (_, c) = creer(
        &etat,
        json!({"name": "Karajan", "rules": [], "expression": "conductor:karajan"}),
    )
    .await;
    let id = c["id"].as_i64().unwrap();
    let maj = |v: Value| {
        let etat = etat.clone();
        async move {
            let corps_json = serde_json::from_value(v).unwrap();
            let r = match super::update_collection(State(etat), Path(id), Json(corps_json)).await {
                Ok(r) => r.into_response(),
                Err(e) => e.into_response(),
            };
            corps(r).await
        }
    };
    let (statut, refus) = maj(json!({"expression": "conductor:(karajan"})).await;
    assert_eq!(statut, axum::http::StatusCode::BAD_REQUEST, "{refus}");
    assert!(refus["position"].is_u64(), "{refus}");
    assert_eq!(
        albums(&etat, id).await,
        noms(&["Cinquième 1963", "Neuvième 1976", "Septième live"]),
        "l'expression d'avant reste"
    );
    // Absente : inchangée.
    let (_, relu) = maj(json!({"name": "Karajan bis"})).await;
    assert_eq!(relu["expression"], "conductor:karajan", "{relu}");
    // `null` : effacée, la collection reprend toute la bibliothèque.
    let (_, relu) = maj(json!({"expression": null})).await;
    assert!(relu["expression"].is_null(), "{relu}");
    assert_eq!(albums(&etat, id).await.len(), 4);
}

#[tokio::test]
async fn l_apercu_evalue_l_expression_7872() {
    let etat = etat();
    let apercu = |v: Value| {
        let etat = etat.clone();
        async move {
            let corps_json = serde_json::from_value(v).unwrap();
            let r = match super::preview_albums(State(etat), ActiveProfile(1), Json(corps_json))
                .await
            {
                Ok(r) => r,
                Err(e) => e.into_response(),
            };
            corps(r).await
        }
    };
    let (statut, v) = apercu(json!({"rules": [], "expression": REQUETE_DE_L_ISSUE})).await;
    assert!(statut.is_success(), "{v}");
    assert_eq!(v["total"], 1, "{v}");
    assert_eq!(v["albums"][0]["title"], "Cinquième 1963");
    let (statut, v) = apercu(json!({"rules": [], "expression": "\"karajan"})).await;
    assert_eq!(statut, axum::http::StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["position"], 0, "{v}");
}
