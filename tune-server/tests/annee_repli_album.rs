//! Go de Bertrand du 13/11 (lot rc12) — une piste SANS année (NULL ou 0)
//! prend, à la LECTURE, l'année de son album : filtre `year` de la liste des
//! pistes, facette Années, règle `year` des playlists intelligentes, carte
//! d'album, répartition par décennie. Rien n'est écrit : `tracks.year` reste
//! NULL ou 0 après coup.
//!
//! Le même scénario tourne sur SQLite (suite ordinaire) et sur PostgreSQL
//! (`--features postgres`, `TUNE_TEST_PG_URL`, étape dédiée de
//! `test-postgres.yml`) : la sous-requête corrélée et les `NULLIF` sur des
//! colonnes INTEGER sont ce que PostgreSQL juge plus sévèrement.

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const MARQUE: &str = "annee-repli-1311";
/// Des années que personne d'autre n'emploie : la base PostgreSQL du CI est
/// partagée par les épreuves.
const ANNEE_ALBUM: i64 = 1811;
const ANNEE_PISTE: i64 = 1812;

async fn appel(state: &AppState, methode: &str, route: &str, corps: Option<Value>) -> Value {
    let app = tune_server::routes::router(state.clone());
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
    let octets = axum::body::to_bytes(reponse.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    assert!(statut.is_success(), "{methode} {route} → {statut} : {texte}");
    serde_json::from_str(&texte).unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

fn titres(liste: &Value) -> Vec<String> {
    let mut t: Vec<String> = liste
        .as_array()
        .unwrap_or_else(|| panic!("une liste : {liste}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .filter(|t| t.starts_with(MARQUE))
        .collect();
    t.sort();
    t
}

fn menage(state: &AppState) {
    let b = &state.backend;
    for sql in [
        format!("DELETE FROM tracks WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

/// | piste | album (année) | `tracks.year` | année lue |
/// |---|---|---|---|
/// | nulle | A (1811) | NULL | 1811 |
/// | zero | A (1811) | 0 | 1811 |
/// | propre | A (1811) | 1812 | 1812 (la sienne d'abord) |
/// | orpheline | B (NULL) | NULL | aucune |
async fn scenario(state: &AppState) {
    menage(state);
    let b = state.backend.clone();
    let nom = format!("{MARQUE} Artiste");
    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    let album = |titre: &str, annee: Option<i64>| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id, year) VALUES (?, ?, ?)",
            &[&titre as &dyn ToSqlValue, &artiste, &annee],
        )
        .expect("album")
    };
    let a = album("A", Some(ANNEE_ALBUM));
    let sans = album("B", None);
    let piste = |album_id: i64, titre: &str, annee: Option<i64>| {
        let titre = format!("{MARQUE} {titre}");
        let chemin = format!("/m/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path, year) VALUES (?, ?, ?, ?, ?)",
            &[&album_id as &dyn ToSqlValue, &artiste, &titre, &chemin, &annee],
        )
        .expect("piste")
    };
    piste(a, "nulle", None);
    piste(a, "zero", Some(0));
    piste(a, "propre", Some(ANNEE_PISTE));
    piste(sans, "orpheline", None);
    let t = |s: &str| format!("{MARQUE} {s}");

    // 1. Le filtre `year` de la liste des pistes.
    let v = appel(state, "GET", &format!("/api/v1/library/tracks?year={ANNEE_ALBUM}&limit=500"), None).await;
    assert_eq!(titres(&v["items"]), vec![t("nulle"), t("zero")], "{v}");
    let v = appel(state, "GET", &format!("/api/v1/library/tracks?year={ANNEE_PISTE}&limit=500"), None).await;
    assert_eq!(titres(&v["items"]), vec![t("propre")], "{v}");

    // 2. La facette Années compte les pistes repliées sur l'album.
    let v = appel(
        state,
        "GET",
        &format!("/api/v1/library/facets?fields=year&year={ANNEE_ALBUM}"),
        None,
    )
    .await;
    let compte = |annee: i64| {
        v["year"]
            .as_array()
            .unwrap_or_else(|| panic!("facette year : {v}"))
            .iter()
            .find(|e| e["value"] == json!(annee.to_string()))
            .map(|e| e["count"].as_i64().unwrap_or(0))
    };
    assert_eq!(compte(ANNEE_ALBUM), Some(2), "{v}");
    assert_eq!(compte(ANNEE_PISTE), Some(1), "{v}");

    // 3. La règle `year` des playlists intelligentes.
    let pl = "/api/v1/library/smart-playlists/preview";
    let r = appel(
        state,
        "POST",
        pl,
        Some(json!({"rules": [{"field": "year", "op": "=", "value": ANNEE_ALBUM}], "match_mode": "all"})),
    )
    .await;
    assert_eq!(titres(&r["tracks"]), vec![t("nulle"), t("zero")], "{r}");
    let r = appel(
        state,
        "POST",
        pl,
        Some(json!({"rules": [{"field": "year", "op": "between", "value": [ANNEE_ALBUM, ANNEE_PISTE]}], "match_mode": "all"})),
    )
    .await;
    assert_eq!(titres(&r["tracks"]), vec![t("nulle"), t("propre"), t("zero")], "{r}");

    // 4. Rien n'est écrit : les pistes gardent leur année vide.
    let lignes = b
        .query_many(
            &format!("SELECT title, year FROM tracks WHERE title LIKE '{MARQUE}%' ORDER BY title"),
            &[],
        )
        .unwrap();
    let lu: Vec<(String, Option<i64>)> = lignes
        .iter()
        .map(|l| (l[0].as_string().unwrap(), l[1].as_i64()))
        .collect();
    assert_eq!(
        lu,
        vec![
            (t("nulle"), None),
            (t("orpheline"), None),
            (t("propre"), Some(ANNEE_PISTE)),
            (t("zero"), Some(0)),
        ]
    );
    menage(state);
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_l_annee_de_l_album_supplee_celle_de_la_piste() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    scenario(&state).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_l_annee_de_l_album_supplee_celle_de_la_piste() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    scenario(&state).await;
}
