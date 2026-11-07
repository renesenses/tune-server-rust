//! La requête avancée de la Bibliothèque sur une VRAIE base PostgreSQL
//! (#7872) : `GET /library/query` (200, 400 avec position, pagination) et
//! une collection intelligente qui porte une `expression` (PG 106), par le
//! vrai routeur.
//!
//! Sur PostgreSQL, le SQL compilé passe par `CAST($n AS TEXT)`,
//! `unaccent`, `NOT COALESCE(EXISTS …)` et, pour la collection, une
//! sous-requête `al.id IN (SELECT t.album_id …)` dont les paramètres liés
//! suivent une clause de règles écrite en littéraux. Même doctrine que
//! `pg_etiquette_collection_5026.rs` : variable ABSENTE ⇒ saut annoncé ;
//! variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use std::collections::BTreeSet;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes.
const MARQUE: &str = "q7872";

const REQUETE_DE_L_ISSUE: &str = "composer:beethoven conductor:karajan year:1960..1969 -genre:live";

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
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT id FROM tracks WHERE file_path LIKE '/{MARQUE}/%')"
        ),
        format!("DELETE FROM tracks WHERE file_path LIKE '/{MARQUE}/%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE} %'"),
        format!("DELETE FROM artists WHERE name = '{MARQUE}'"),
        format!("DELETE FROM smart_collections WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn appel(
    state: &AppState,
    methode: &str,
    route: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let requete = Request::builder()
        .method(methode)
        .uri(route)
        .header(header::CONTENT_TYPE, "application/json");
    let requete = match corps {
        Some(c) => requete.body(Body::from(c.to_string())).unwrap(),
        None => requete.body(Body::empty()).unwrap(),
    };
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    let json = serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} → {statut} : JSON illisible ({e}) : {texte}"));
    (statut, json)
}

fn requete(q: &str, suite: &str) -> String {
    format!("/api/v1/library/query?q={}{suite}", urlencoding::encode(q))
}

/// Nos pistes seulement : la base est partagée avec les autres bancs.
fn nos_titres(items: &Value) -> BTreeSet<String> {
    items
        .as_array()
        .expect("items")
        .iter()
        .map(|p| p["title"].as_str().unwrap_or_default().to_string())
        .filter(|t| t.starts_with(MARQUE))
        .collect()
}

fn noms(liste: &[&str]) -> BTreeSet<String> {
    liste.iter().map(|s| format!("{MARQUE} {s}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_7872_requete_avancee_et_collection_a_expression() {
    let Some(url) = std::env::var("TUNE_TEST_PG_URL").ok() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&MARQUE as &dyn ToSqlValue],
        )
        .expect("artiste");
    let album = |titre: &str, annee: i64| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id, year, source) VALUES (?, ?, ?, 'local')",
            &[&titre as &dyn ToSqlValue, &artiste, &annee],
        )
        .expect("album")
    };
    let cinquieme = album("Cinquième 1963", 1963);
    let live = album("Septième live", 1965);
    let neuvieme = album("Neuvième 1976", 1976);
    // (album, titre, année, genre, compositeur, chef)
    let pistes = [
        (
            cinquieme,
            "Allegro con brio",
            1963,
            "Classique",
            "Ludwig van Beethoven",
            "Herbert von Karajan",
        ),
        (
            cinquieme,
            "Andante con moto",
            1963,
            "Classique",
            "Ludwig van Beethoven",
            "Herbert von Karajan",
        ),
        (
            cinquieme,
            "Allegro",
            1963,
            "Classique",
            "Ludwig van Beethoven",
            "Herbert von Karajan",
        ),
        (
            live,
            "Poco sostenuto",
            1965,
            "Live",
            "Ludwig van Beethoven",
            "Herbert von Karajan",
        ),
        (
            neuvieme,
            "Ode",
            1976,
            "Classique",
            "Ludwig van Beethoven",
            "Karl Böhm",
        ),
    ];
    for (n, (album_id, titre, annee, genre, compositeur, chef)) in pistes.iter().enumerate() {
        let titre = format!("{MARQUE} {titre}");
        let chemin = format!("/{MARQUE}/{n}.flac");
        let piste = b
            .execute_returning_id(
                "INSERT INTO tracks (album_id, artist_id, title, file_path, year, genre) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    album_id as &dyn ToSqlValue,
                    &artiste,
                    &titre,
                    &chemin,
                    annee,
                    genre,
                ],
            )
            .expect("piste");
        for (nom, role) in [(compositeur, "composer"), (chef, "conductor")] {
            b.execute(
                "INSERT INTO track_credits (track_id, artist_name, role) VALUES (?, ?, ?)",
                &[&piste as &dyn ToSqlValue, nom, &role],
            )
            .expect("crédit");
        }
    }

    // 1. La route : la requête de l'issue rend les trois pistes de 1963.
    let (statut, corps) = appel(&state, "GET", &requete(REQUETE_DE_L_ISSUE, ""), None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        nos_titres(&corps["items"]),
        noms(&["Allegro con brio", "Andante con moto", "Allegro"]),
        "{corps}"
    );
    // Accents et casse ignorés sur PG aussi (`unaccent`).
    let (_, corps) = appel(&state, "GET", &requete("conductor:BOHM", ""), None).await;
    assert_eq!(nos_titres(&corps["items"]), noms(&["Ode"]), "{corps}");

    // 2. Pagination : nos pistes de Karajan, deux par page.
    let q = format!("conductor:karajan title:{MARQUE}");
    let mut vus = BTreeSet::new();
    for decalage in [0, 2] {
        let (statut, corps) = appel(
            &state,
            "GET",
            &requete(&q, &format!("&limit=2&offset={decalage}")),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["total"], 4, "{corps}");
        assert_eq!(corps["has_more"], decalage == 0, "{corps}");
        for p in corps["items"].as_array().unwrap() {
            assert!(vus.insert(p["id"].as_i64().unwrap()), "page recouverte");
        }
    }
    assert_eq!(vus.len(), 4);

    // 3. Erreur 400 avec sa position, jamais 500.
    let (statut, corps) = appel(&state, "GET", &requete("year:1963 ratng:5", ""), None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["position"], 10, "{corps}");

    // 4. Une collection qui porte l'expression, avec une règle en littéraux
    //    AVANT les paramètres liés de l'expression.
    let (statut, c) = appel(
        &state,
        "POST",
        "/api/v1/library/smart-collections",
        Some(json!({
            "name": format!("{MARQUE} Karajan"),
            "rules": [{"field": "artist", "op": "=", "value": MARQUE}],
            "expression": "conductor:karajan -genre:live",
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{c}");
    assert_eq!(c["expression"], "conductor:karajan -genre:live");
    let id = c["id"].as_i64().unwrap();
    let (statut, albums) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/smart-collections/{id}/albums"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{albums}");
    let titres: BTreeSet<String> = albums
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(titres, noms(&["Cinquième 1963"]), "{albums}");

    // La liste des collections compte avec la même expression.
    let (_, liste) = appel(&state, "GET", "/api/v1/library/smart-collections", None).await;
    let ligne = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .expect("collection listée")
        .clone();
    assert_eq!(ligne["album_count"], 1, "{ligne}");
    assert_eq!(ligne["track_count"], 3, "{ligne}");

    // Mise à jour de l'expression, puis aperçu.
    let (statut, maj) = appel(
        &state,
        "PUT",
        &format!("/api/v1/library/smart-collections/{id}"),
        Some(json!({"expression": "composer:beethoven"})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{maj}");
    let (_, albums) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/smart-collections/{id}/albums"),
        None,
    )
    .await;
    assert_eq!(albums.as_array().unwrap().len(), 3, "{albums}");
    let (statut, apercu) = appel(
        &state,
        "POST",
        "/api/v1/library/smart-collections/preview",
        Some(json!({"rules": [], "expression": format!("conductor:bohm album:{MARQUE}")})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["total"], 1, "{apercu}");

    menage(&state);
}
