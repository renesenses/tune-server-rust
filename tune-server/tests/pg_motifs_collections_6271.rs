//! Collections intelligentes : « finit par », et les motifs (« contient »,
//! « ne contient pas », « commence par », « finit par ») sur une colonne
//! NUMÉRIQUE, sur une VRAIE base PostgreSQL (#6271).
//!
//! `LOWER(entier)` passait sur SQLite et PostgreSQL le refusait (`function
//! lower(integer) does not exist`) : l'aperçu tombait. Le nombre se lit par
//! `CAST(… AS TEXT)`. Même doctrine que `pg_ne_contient_pas_6250.rs` :
//! variable ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le
//! test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes — et chaque règle est bornée
/// à nos albums, puisque « ne contient pas » retiendrait ceux des autres.
const MARQUE: &str = "motifs-collections-6271";

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

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
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn apercu(state: &AppState, route: &str, regles: Value) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let corps = json!({"rules": regles, "match_mode": "all"});
    let requete = Request::builder()
        .method("POST")
        .uri(route)
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
        "POST {route} → {statut} sur PostgreSQL (#6271) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

fn titres(liste: &Value) -> Vec<String> {
    let mut t: Vec<String> = liste
        .as_array()
        .expect("une liste")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6271_motifs_des_collections_sur_un_nombre_et_finit_par() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let nom = format!("{MARQUE} Miles");
    let miles = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    let album = |titre: &str, annee: i64| {
        let titre = format!("{MARQUE} {titre}");
        let id = b
            .execute_returning_id(
                "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
                &[&titre as &dyn ToSqlValue, &miles],
            )
            .expect("album");
        let chemin = format!("/m/{MARQUE}/{id}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path, year, sample_rate, bit_depth) \
             VALUES (?, ?, ?, ?, ?, 96000, 24)",
            &[&id as &dyn ToSqlValue, &miles, &titre, &chemin, &annee],
        )
        .expect("piste");
    };
    album("Kind of Blue", 1959);
    album("Bitches Brew", 1970);

    let a = |t: &str| format!("{MARQUE} {t}");
    let coll = "/api/v1/library/smart-collections/preview";
    let borne = json!({"field": "title", "op": "contains", "value": MARQUE});
    let r = |champ: &str, op: &str, v: &str| json!([borne, {"field": champ, "op": op, "value": v}]);

    let x = apercu(&state, coll, r("year", "contains", "95")).await;
    assert_eq!(titres(&x["albums"]), vec![a("Kind of Blue")], "{x}");
    let x = apercu(&state, coll, r("year", "not_contains", "95")).await;
    assert_eq!(titres(&x["albums"]), vec![a("Bitches Brew")], "{x}");
    let x = apercu(&state, coll, r("year", "starts_with", "197")).await;
    assert_eq!(titres(&x["albums"]), vec![a("Bitches Brew")], "{x}");
    let x = apercu(&state, coll, r("year", "ends_with", "59")).await;
    assert_eq!(titres(&x["albums"]), vec![a("Kind of Blue")], "{x}");
    let x = apercu(&state, coll, r("title", "ends_with", "BREW")).await;
    assert_eq!(titres(&x["albums"]), vec![a("Bitches Brew")], "{x}");
    // Chaque colonne numérique, chaque motif : l'aperçu répond.
    for champ in [
        "year",
        "original_year",
        "sample_rate",
        "bit_depth",
        "track_count",
        "duration",
        "track_number",
        "disc_number",
        "bpm",
        "track_rating",
    ] {
        for op in ["contains", "not_contains", "starts_with", "ends_with"] {
            apercu(&state, coll, r(champ, op, "9")).await;
        }
    }

    menage(&state);
}
