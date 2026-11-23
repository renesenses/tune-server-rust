//! #9283 (forum, « Collection intelligente et caractères spéciaux ») — une
//! règle « Blue Öyster Cult » trouve l'artiste quelle que soit sa graphie en
//! base (sans accent, composée NFC, décomposée NFD), sur une VRAIE base
//! PostgreSQL, par le vrai routeur. La même épreuve sur SQLite vit dans
//! `tune-smart-http/src/accents_regles_9283_tests.rs`.
//!
//! Même doctrine que `pg_motifs_collections_6271.rs` : variable ABSENTE ⇒
//! saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use std::collections::BTreeSet;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL. Chaque règle est bornée à nos albums par leur titre.
const MARQUE: &str = "accents-collections-9283";

const NFC: &str = "Blue \u{00D6}yster Cult";
const NFD: &str = "Blue O\u{0308}yster Cult";

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

/// Les artistes du banc, par leur nom exact (la règle « = » l'exige).
const ARTISTES: [&str; 4] = ["Blue Oyster Cult", NFC, NFD, "Mot\u{00F6}rhead"];

/// Retire nos pistes et nos albums, puis ceux de nos artistes que plus
/// aucun album ni aucune piste ne désigne : un homonyme d'un autre banc,
/// encore utilisé, reste en place.
fn menage(state: &AppState) {
    let b = &state.backend;
    for sql in [
        format!(
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
    for nom in ARTISTES {
        b.execute(
            "DELETE FROM artists ar WHERE ar.name = ? \
             AND NOT EXISTS (SELECT 1 FROM albums al WHERE al.artist_id = ar.id) \
             AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.artist_id = ar.id)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap_or_else(|e| panic!("ménage de l'artiste {nom:?} : {e}"));
    }
}

async fn apercu(state: &AppState, regle: Value) -> BTreeSet<String> {
    let app: Router = tune_server::routes::router(state.clone());
    let borne = json!({"field": "title", "op": "contains", "value": MARQUE});
    let corps = json!({"rules": [borne, regle], "match_mode": "all"});
    let requete = Request::builder()
        .method("POST")
        .uri("/api/v1/library/smart-collections/preview")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    assert!(
        statut.is_success(),
        "aperçu → {statut} sur PostgreSQL (#9283) : {texte}"
    );
    let v: Value = serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("aperçu : JSON illisible ({e}) : {texte}"));
    v["albums"]
        .as_array()
        .expect("une liste d'albums")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_9283_une_regle_accentuee_trouve_toutes_les_graphies() {
    let Some(url) = std::env::var("TUNE_TEST_PG_URL").ok() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();
    let a = |t: &str| format!("{MARQUE} {t}");

    for (n, (artiste, album)) in ARTISTES
        .into_iter()
        .zip(["Sans accent", "Composé", "Décomposé", "Témoin"])
        .enumerate()
    {
        let artiste_id = b
            .execute_returning_id(
                "INSERT INTO artists (name) VALUES (?)",
                &[&artiste as &dyn ToSqlValue],
            )
            .expect("artiste");
        let titre = a(album);
        let album_id = b
            .execute_returning_id(
                "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
                &[&titre as &dyn ToSqlValue, &artiste_id],
            )
            .expect("album");
        let chemin = format!("/m/{MARQUE}/{n}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path) VALUES (?, ?, ?, ?)",
            &[&album_id as &dyn ToSqlValue, &artiste_id, &titre, &chemin],
        )
        .expect("piste");
    }

    let les_trois: BTreeSet<String> = ["Composé", "Décomposé", "Sans accent"]
        .into_iter()
        .map(a)
        .collect();
    for valeur in [NFC, NFD, "blue \u{00F6}yster cult", "Blue Oyster Cult"] {
        for op in ["contains", "=", "starts_with"] {
            let trouves = apercu(
                &state,
                json!({"field": "artist", "op": op, "value": valeur}),
            )
            .await;
            assert_eq!(
                trouves, les_trois,
                "#9283 : artiste {op} {valeur:?} doit trouver les trois graphies"
            );
        }
    }
    let temoin: BTreeSet<String> = [a("Témoin")].into_iter().collect();
    for op in ["!=", "not_contains"] {
        let trouves = apercu(&state, json!({"field": "artist", "op": op, "value": NFC})).await;
        assert_eq!(trouves, temoin, "#9283 : artiste {op} {NFC:?}");
    }

    menage(&state);
}
