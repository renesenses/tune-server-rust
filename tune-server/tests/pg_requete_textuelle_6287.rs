//! #6287 — la requête TEXTUELLE des règles intelligentes sur une VRAIE base
//! PostgreSQL. Le SQL neuf est celui des combinaisons : `NOT COALESCE((a),
//! 1 = 0)` pour la négation, les parenthèses du ET et du OU, et la règle
//! crédit dans une sous-requête. Même doctrine que `pg_ne_contient_pas_6250.rs` :
//! variable ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le
//! test TOMBE.

#![cfg(feature = "postgres")]

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const MARQUE: &str = "requete-textuelle-6287";

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &AppState) {
    let b = &state.backend;
    for sql in [
        format!(
            "DELETE FROM track_credits WHERE track_id IN (SELECT id FROM tracks WHERE file_path LIKE '/m/{MARQUE}/%')"
        ),
        format!("DELETE FROM tracks WHERE file_path LIKE '/m/{MARQUE}/%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn requete(state: &AppState, q: &str) -> Value {
    let app = tune_server::routes::router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/library/smart-playlists/requete")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "q": q }).to_string()))
        .unwrap();
    let rep = app.oneshot(req).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    assert!(
        statut.is_success(),
        "{q} → {statut} sur PostgreSQL : {texte}"
    );
    serde_json::from_str(&texte).unwrap()
}

fn titres(r: &Value) -> Vec<String> {
    let mut t: Vec<String> = r["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .map(|p| p["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6287_negation_combinaisons_et_credits() {
    let Some(url) = std::env::var("TUNE_TEST_PG_URL").ok() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();
    let nom = format!("{MARQUE} Miles");
    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap();
    let titre_album = format!("{MARQUE} Album");
    let album = b
        .execute_returning_id(
            "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
            &[&titre_album as &dyn ToSqlValue, &artiste],
        )
        .unwrap();
    let piste = |titre: &str, genre: Option<&str>| {
        let chemin = format!("/m/{MARQUE}/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, genre, file_path) VALUES (?, ?, ?, ?, ?)",
            &[&album as &dyn ToSqlValue, &artiste, &titre, &genre, &chemin],
        )
        .unwrap()
    };
    let p1 = piste("P1", Some("Jazz"));
    piste("P2", Some("Rock"));
    piste("P3", None);
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, instrument) VALUES (?, 'Bill Evans', 'performer', 'piano')",
        &[&p1 as &dyn ToSqlValue],
    )
    .unwrap();

    let borne = format!("album:\"{MARQUE}\"");
    // La négation garde la piste SANS genre : complément exact.
    let r = requete(&state, &format!("{borne} non genre:rock")).await;
    assert_eq!(titres(&r), vec!["P1", "P3"], "{r}");
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    // Crédit, combinaisons, parenthèses.
    let r = requete(
        &state,
        &format!("{borne} (credit(artiste: \"Bill Evans\", instrument: piano) ou genre:rock)"),
    )
    .await;
    assert_eq!(titres(&r), vec!["P1", "P2"], "{r}");
    let r = requete(
        &state,
        &format!("{borne} non credit(instrument: piano) jamais_ecoute"),
    )
    .await;
    assert_eq!(titres(&r), vec!["P2", "P3"], "{r}");

    menage(&state);
}
