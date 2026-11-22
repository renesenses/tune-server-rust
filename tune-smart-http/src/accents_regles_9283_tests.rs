//! #9283 (forum, « Collection intelligente et caractères spéciaux ») — une
//! règle « Blue Öyster Cult » ne trouvait rien, la même sans accent trouvait,
//! et la recherche de la bibliothèque trouvait les deux. Sur SQLite ; la même
//! épreuve sur PostgreSQL vit dans
//! `tune-server/tests/pg_collection_accents_9283.rs`.
//!
//! Banc : le même artiste écrit de trois façons, plus un témoin qui ne doit
//! jamais entrer.
//!
//! | album      | artiste en base                                  |
//! |------------|--------------------------------------------------|
//! | Sans accent| `Blue Oyster Cult`                               |
//! | Composé    | `Blue Öyster Cult` (NFC, « Ö » en un caractère)  |
//! | Décomposé  | `Blue O\u{0308}yster Cult` (NFD, balises macOS)  |
//! | Témoin     | `Motörhead`                                      |

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_http_types::ActiveProfile;

use crate::SmartHttpState;

const NFC: &str = "Blue \u{00D6}yster Cult";
const NFD: &str = "Blue O\u{0308}yster Cult";

fn etat() -> SmartHttpState {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    db.execute_batch("DELETE FROM smart_collections;")
        .expect("table vide");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    for (n, (artiste, album)) in [
        ("Blue Oyster Cult", "Sans accent"),
        (NFC, "Composé"),
        (NFD, "Décomposé"),
        ("Mot\u{00F6}rhead", "Témoin"),
    ]
    .into_iter()
    .enumerate()
    {
        let artiste_id = backend
            .execute_returning_id(
                "INSERT INTO artists (name) VALUES ($1)",
                &[&artiste as &dyn ToSqlValue],
            )
            .expect("artiste");
        let album_id = backend
            .execute_returning_id(
                "INSERT INTO albums (title, artist_id, source) VALUES ($1, $2, 'local')",
                &[&album as &dyn ToSqlValue, &artiste_id as &dyn ToSqlValue],
            )
            .expect("album");
        let chemin = format!("/b9283/{n}.flac");
        backend
            .execute_returning_id(
                "INSERT INTO tracks (title, album_id, artist_id, file_path, genre) \
                 VALUES ($1, $2, $3, $4, 'Rock')",
                &[
                    &album as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &artiste_id as &dyn ToSqlValue,
                    &chemin as &dyn ToSqlValue,
                ],
            )
            .expect("piste");
    }
    SmartHttpState::new(backend)
}

async fn apercu(etat: &SmartHttpState, regle: Value) -> BTreeSet<String> {
    let corps_json =
        serde_json::from_value(json!({"rules": [regle], "match_mode": "all"})).expect("aperçu");
    let reponse = match super::preview_albums(
        State(etat.clone()),
        ActiveProfile(1),
        Json(corps_json),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert!(statut.is_success(), "aperçu {statut} : {v}");
    v["albums"]
        .as_array()
        .expect("une liste d'albums")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn les_trois() -> BTreeSet<String> {
    ["Composé", "Décomposé", "Sans accent"]
        .into_iter()
        .map(String::from)
        .collect()
}

#[tokio::test]
async fn une_regle_accentuee_trouve_l_artiste_quelle_que_soit_sa_graphie_9283() {
    let etat = etat();
    for valeur in [NFC, NFD, "blue \u{00F6}yster cult", "Blue Oyster Cult"] {
        for op in ["contains", "=", "starts_with"] {
            let trouves =
                apercu(&etat, json!({"field": "artist", "op": op, "value": valeur})).await;
            assert_eq!(
                trouves,
                les_trois(),
                "#9283 : artiste {op} {valeur:?} doit trouver les trois graphies"
            );
        }
    }
    // « ends_with » sur la partie accentuée.
    let trouves = apercu(
        &etat,
        json!({"field": "artist", "op": "ends_with", "value": "\u{00D6}yster Cult"}),
    )
    .await;
    assert_eq!(trouves, les_trois());
}

#[tokio::test]
async fn la_negation_ecarte_aussi_les_trois_graphies_9283() {
    let etat = etat();
    let temoin: BTreeSet<String> = ["Témoin".to_string()].into_iter().collect();
    for op in ["!=", "not_contains"] {
        let trouves = apercu(&etat, json!({"field": "artist", "op": op, "value": NFC})).await;
        assert_eq!(trouves, temoin, "#9283 : artiste {op} {NFC:?}");
    }
}
