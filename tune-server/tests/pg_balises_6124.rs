//! #6124 — la date de sortie originale et la version sur PostgreSQL : les
//! deux règles intelligentes (« Année de sortie originale », « Version »)
//! par les aperçus des collections et des playlists, la concordance des
//! pistes sur la fiche d'album et les deux champs sur la fiche d'une piste,
//! par le VRAI routeur.
//!
//! Ce qui n'existe que sur PostgreSQL : la sous-requête corrélée de la
//! version sur `track_metadata` (clé TEXT), le `CAST` de
//! `albums.original_year` (INTEGER depuis la PG 010), le `COUNT(*)` BIGINT de
//! la concordance comparé au nombre de pistes.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const MARQUE: &str = "balises-6124";

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
            "DELETE FROM track_metadata WHERE track_id IN (SELECT t.id FROM tracks t \
             JOIN albums a ON a.id = t.album_id WHERE a.title LIKE '{MARQUE}%')"
        ),
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

async fn appel(state: &AppState, methode: &str, route: &str, corps: Option<Value>) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let mut requete = Request::builder().method(methode).uri(route);
    let body = match corps {
        Some(c) => {
            requete = requete.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.oneshot(requete.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    assert!(
        statut.is_success(),
        "{methode} {route} → {statut} sur PostgreSQL (#6124) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

async fn apercu(state: &AppState, route: &str, regle: Value) -> Value {
    appel(
        state,
        "POST",
        route,
        Some(json!({"rules": [regle], "match_mode": "all"})),
    )
    .await
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
async fn pg_6124_date_originale_et_version() {
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
    let album = |titre: &str, annee: Option<i64>| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id, original_year) VALUES (?, ?, ?)",
            &[&titre as &dyn ToSqlValue, &miles, &annee],
        )
        .expect("album")
    };
    let kind = album("Kind of Blue", Some(1959));
    let brew = album("Bitches Brew", Some(1970));
    let piste = |album_id: i64, titre: &str| {
        let chemin = format!("/m/{MARQUE}/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path) VALUES (?, ?, ?, ?)",
            &[&album_id as &dyn ToSqlValue, &miles, &titre, &chemin],
        )
        .expect("piste")
    };
    let so_what = piste(kind, "So What");
    let blue = piste(kind, "Blue in Green");
    let spanish = piste(brew, "Spanish Key");
    let meta = |track_id: i64, cle: &str, valeur: &str| {
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
            &[&track_id as &dyn ToSqlValue, &cle, &valeur],
        )
        .expect("track_metadata");
    };
    meta(so_what, "original_date", "1959-08-17");
    meta(blue, "original_date", "1959-08-17");
    meta(so_what, "version", "Remaster 2009");
    meta(blue, "version", "Remaster 2009");
    meta(spanish, "version", "Mono");

    // Collections : année originale, puis version.
    let coll = "/api/v1/library/smart-collections/preview";
    let r = apercu(
        &state,
        coll,
        json!({"field": "original_year", "op": "<", "value": 1960}),
    )
    .await;
    assert!(
        titres(&r["albums"]).contains(&format!("{MARQUE} Kind of Blue"))
            && !titres(&r["albums"]).contains(&format!("{MARQUE} Bitches Brew")),
        "{r}"
    );
    let r = apercu(
        &state,
        coll,
        json!({"field": "version", "op": "contains", "value": "MONO"}),
    )
    .await;
    let t = titres(&r["albums"]);
    assert!(t.contains(&format!("{MARQUE} Bitches Brew")), "{r}");
    assert!(!t.contains(&format!("{MARQUE} Kind of Blue")), "{r}");

    // Playlists : version, puis année originale.
    let pl = "/api/v1/library/smart-playlists/preview";
    let r = apercu(
        &state,
        pl,
        json!({"field": "version", "op": "=", "value": "remaster 2009"}),
    )
    .await;
    assert_eq!(
        titres(&r["tracks"]),
        vec!["Blue in Green".to_string(), "So What".to_string()],
        "{r}"
    );
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    let r = apercu(
        &state,
        pl,
        json!({"field": "original_year", "op": "between", "value": [1965, 1975]}),
    )
    .await;
    assert!(
        titres(&r["tracks"]).contains(&"Spanish Key".to_string())
            && !titres(&r["tracks"]).contains(&"So What".to_string()),
        "{r}"
    );

    // Fiche d'album : la date et la version sur lesquelles les pistes
    // concordent.
    let a = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{kind}"),
        None,
    )
    .await;
    assert_eq!(a["original_date"], "1959-08-17", "{a}");
    assert_eq!(a["version"], "Remaster 2009", "{a}");

    // Fiche d'une piste.
    let t = appel(
        &state,
        "GET",
        &format!("/api/v1/library/tracks/{so_what}"),
        None,
    )
    .await;
    assert_eq!(t["original_date"], "1959-08-17", "{t}");
    assert_eq!(t["version"], "Remaster 2009", "{t}");

    menage(&state);
}
