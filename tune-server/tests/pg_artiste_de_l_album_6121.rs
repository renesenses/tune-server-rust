//! La règle « Artiste de l'album » des collections ET des playlists
//! intelligentes, sur une VRAIE base PostgreSQL (#6121, forum 2218).
//!
//! Le SQL est neuf des deux côtés : `COALESCE(NULLIF(t.album_artist, ''), …)`
//! dans les collections, et une sous-requête corrélée sur `al.artist_id` dans
//! les playlists. L'épreuve rejoue les deux aperçus par le vrai routeur. Même
//! doctrine que `pg_etiquette_collection_5026.rs` : variable ABSENTE ⇒ saut
//! annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes.
const MARQUE: &str = "artiste-album-6121";

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

async fn apercu(state: &AppState, route: &str, regle: Value) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let corps = json!({"rules": [regle], "match_mode": "all"});
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
        "POST {route} → {statut} sur PostgreSQL (#6121) : {texte}"
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
async fn pg_6121_artiste_de_l_album_dans_les_collections_et_les_playlists() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let artiste = |nom: &str| {
        let nom = format!("{MARQUE} {nom}");
        b.execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste")
    };
    let miles = artiste("Miles");
    let divers = artiste("Divers");
    let evans = artiste("Evans");
    let album = |titre: &str, artiste_id: i64| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
            &[&titre as &dyn ToSqlValue, &artiste_id],
        )
        .expect("album")
    };
    let kind = album("Kind of Blue", miles);
    let ballads = album("Ballads", divers);
    let piste = |album_id: i64, artiste_id: i64, balise: Option<&str>, titre: &str| {
        let chemin = format!("/m/{MARQUE}/{titre}.flac");
        let balise = balise.map(|n| format!("{MARQUE} {n}"));
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, album_artist, title, file_path) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                &album_id as &dyn ToSqlValue,
                &artiste_id,
                &balise,
                &titre,
                &chemin,
            ],
        )
        .expect("piste")
    };
    // Kind of Blue : pas de balise ALBUMARTIST, l'artiste de l'album répond.
    piste(kind, miles, None, "So What");
    // Ballads : compilation balisée, dont une piste de Miles.
    piste(ballads, evans, Some("Divers"), "Peace Piece");
    piste(ballads, miles, Some("Divers"), "Flamenco Sketches");

    let regle = |op: &str, nom: &str| json!({"field": "album_artist", "op": op, "value": format!("{MARQUE} {nom}")});

    // 1. Collections : l'album de la balise, puis le repli sur l'artiste.
    let coll = "/api/v1/library/smart-collections/preview";
    let r = apercu(&state, coll, regle("=", "divers")).await;
    assert_eq!(
        titres(&r["albums"]),
        vec![format!("{MARQUE} Ballads")],
        "{r}"
    );
    let r = apercu(&state, coll, regle("=", "Miles")).await;
    assert_eq!(
        titres(&r["albums"]),
        vec![format!("{MARQUE} Kind of Blue")],
        "{r}"
    );
    let r = apercu(&state, coll, regle("contains", "DIV")).await;
    assert_eq!(r["total"], 1, "{r}");

    // 2. Playlists : la sous-requête corrélée sur l'album de la piste.
    let pl = "/api/v1/library/smart-playlists/preview";
    let r = apercu(&state, pl, regle("=", "Divers")).await;
    assert_eq!(
        titres(&r["tracks"]),
        vec!["Flamenco Sketches".to_string(), "Peace Piece".to_string()],
        "{r}"
    );
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    let r = apercu(&state, pl, regle("=", "miles")).await;
    assert_eq!(titres(&r["tracks"]), vec!["So What".to_string()], "{r}");

    menage(&state);
}

/// #6232 (forum 2218) — « Interprète » : la balise ARTIST de la piste
/// (`track_metadata['track_artist']`), à défaut l'artiste de la piste, et une
/// tenue d'artiste qui l'emporte (`LIKE` sur le JSON de `champs_tenus`). Le
/// SQL est neuf : sous-requêtes corrélées dans un `CASE`, lues par les deux
/// moteurs. Même routeur, même étape de CI que #6121.
#[tokio::test(flavor = "multi_thread")]
async fn pg_6232_interprete_dans_les_collections_et_les_playlists() {
    const M: &str = "interprete-6232";
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b = state.backend.clone();
    let menage_6232 = || {
        for sql in [
            format!(
                "DELETE FROM track_metadata WHERE track_id IN (SELECT t.id FROM tracks t \
                 JOIN albums al ON t.album_id = al.id WHERE al.title LIKE '{M}%')"
            ),
            format!(
                "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{M}%')"
            ),
            format!("DELETE FROM albums WHERE title LIKE '{M}%'"),
            format!("DELETE FROM artists WHERE name LIKE '{M}%'"),
        ] {
            b.execute(&sql, &[])
                .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
        }
    };
    menage_6232();

    let beatles_nom = format!("{M} Beatles");
    let beatles = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&beatles_nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    let titre_album = format!("{M} Abbey Road");
    let album = b
        .execute_returning_id(
            "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
            &[&titre_album as &dyn ToSqlValue, &beatles],
        )
        .expect("album");
    let piste = |titre: &str| {
        let chemin = format!("/m/{M}/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path) VALUES (?, ?, ?, ?)",
            &[&album as &dyn ToSqlValue, &beatles, &titre, &chemin],
        )
        .expect("piste")
    };
    let come = piste("Come Together");
    piste("Something");
    let balise = format!("{M} Beatles & Genesis");
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'track_artist', ?)",
        &[&come as &dyn ToSqlValue, &balise],
    )
    .expect("balise ARTIST");

    let regle = |op: &str, v: &str| json!({"field": "track_artist", "op": op, "value": v});
    let coll = "/api/v1/library/smart-collections/preview";
    let pl = "/api/v1/library/smart-playlists/preview";

    // 1. La balise d'UNE piste amène l'album, et la piste.
    let r = apercu(&state, coll, regle("contains", "genesis")).await;
    assert_eq!(titres(&r["albums"]), vec![titre_album.clone()], "{r}");
    let r = apercu(&state, pl, regle("contains", "genesis")).await;
    assert_eq!(
        titres(&r["tracks"]),
        vec!["Come Together".to_string()],
        "{r}"
    );
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    // 2. Sans balise : l'artiste de la piste répond.
    let r = apercu(&state, pl, regle("=", &beatles_nom)).await;
    assert_eq!(titres(&r["tracks"]), vec!["Something".to_string()], "{r}");
    // 3. « ne contient pas » : aucune piste de l'album ne contient.
    let r = apercu(&state, coll, regle("not_contains", "genesis")).await;
    assert!(!titres(&r["albums"]).contains(&titre_album), "{r}");
    // 4. Un artiste tenu à la main l'emporte sur la balise.
    let tenue = format!(r#"{{"chemin":"/m/{M}/Come Together.flac","artist_id":{beatles}}}"#);
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'champs_tenus', ?)",
        &[&come as &dyn ToSqlValue, &tenue],
    )
    .expect("tenue");
    let r = apercu(&state, pl, regle("contains", "genesis")).await;
    assert_eq!(titres(&r["tracks"]), Vec::<String>::new(), "{r}");

    menage_6232();
}
