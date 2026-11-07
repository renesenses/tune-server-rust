//! #7872 — `GET /api/v1/library/query`, la requête avancée de la
//! Bibliothèque.
//!
//! Banc : un chef, un compositeur, sept pistes sur trois albums.
//!
//! | piste              | album          | année | genre     | compositeur | chef    |
//! |--------------------|----------------|-------|-----------|-------------|---------|
//! | Allegro con brio   | Cinquième 1963 | 1963  | Classique | Beethoven   | Karajan |
//! | Andante con moto   | Cinquième 1963 | 1963  | Classique | Beethoven   | Karajan |
//! | Allegro            | Cinquième 1963 | 1963  | Classique | Beethoven   | Karajan |
//! | Finale             | Cinquième 1963 | 1963  | Classique | Beethoven   | Karajan |
//! | Poco sostenuto     | Septième live  | 1965  | Live      | Beethoven   | Karajan |
//! | Allegretto         | Septième 1976  | 1976  | Classique | Beethoven   | Karajan |
//! | Adagio             | Septième 1976  | 1976  | Classique | Mahler      | Böhm    |
//!
//! * `la_requete_de_l_issue_rend_les_pistes_attendues_7872` : la requête de
//!   l'issue rend les quatre pistes de 1963, sous la forme des autres listes ;
//! * `une_requete_mal_formee_rend_400_avec_sa_position_7872` : jamais 500 ;
//! * `la_requete_se_pagine_7872` : `limit`, `offset`, `total`, `has_more`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::BTreeSet;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

use crate::state::AppState;

/// (titre, album, année, genre, compositeur, chef)
const PISTES: [(&str, &str, i64, &str, &str, &str); 7] = [
    (
        "Allegro con brio",
        "Cinquième 1963",
        1963,
        "Classique",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Andante con moto",
        "Cinquième 1963",
        1963,
        "Classique",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Allegro",
        "Cinquième 1963",
        1963,
        "Classique",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Finale",
        "Cinquième 1963",
        1963,
        "Classique",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Poco sostenuto",
        "Septième live",
        1965,
        "Live",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Allegretto",
        "Septième 1976",
        1976,
        "Classique",
        "Ludwig van Beethoven",
        "Herbert von Karajan",
    ),
    (
        "Adagio",
        "Septième 1976",
        1976,
        "Classique",
        "Gustav Mahler",
        "Karl Böhm",
    ),
];

const REQUETE_DE_L_ISSUE: &str = "composer:beethoven conductor:karajan year:1960..1969 -genre:live";

fn banc() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    db.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Berliner Philharmoniker')",
        &[],
    )
    .unwrap();
    let mut albums: Vec<(&str, i64)> = Vec::new();
    for (n, (titre, album, annee, genre, compositeur, chef)) in PISTES.iter().enumerate() {
        let album_id = match albums.iter().find(|(a, _)| a == album) {
            Some((_, id)) => *id,
            None => {
                db.execute(
                    "INSERT INTO albums (title, artist_id, source, year) VALUES (?, 1, 'local', ?)",
                    &[album as &dyn ToSqlValue, annee],
                )
                .unwrap();
                let id = db.last_insert_rowid();
                albums.push((album, id));
                id
            }
        };
        let chemin = format!("/banc7872/{n}.flac");
        let numero = n as i64 + 1;
        db.execute(
            "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path, \
             file_mtime, duration_ms, format, year, genre) \
             VALUES (?, ?, 1, ?, ?, 1000.0, 30000, 'flac', ?, ?)",
            &[
                titre as &dyn ToSqlValue,
                &album_id,
                &numero,
                &chemin,
                annee,
                genre,
            ],
        )
        .unwrap();
        let piste = db.last_insert_rowid();
        for (nom, role) in [(compositeur, "composer"), (chef, "conductor")] {
            db.execute(
                "INSERT INTO track_credits (track_id, artist_name, role) VALUES (?, ?, ?)",
                &[&piste as &dyn ToSqlValue, nom, &role],
            )
            .unwrap();
        }
    }
    crate::routes::router(state)
}

async fn appel(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    (statut, json)
}

fn encoder(q: &str) -> String {
    urlencoding::encode(q).into_owned()
}

fn titres(corps: &Value) -> BTreeSet<String> {
    corps["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|p| p["title"].as_str().expect("title").to_string())
        .collect()
}

#[tokio::test]
async fn la_requete_de_l_issue_rend_les_pistes_attendues_7872() {
    let app = banc();
    let (statut, corps) = appel(
        &app,
        &format!("/api/v1/library/query?q={}", encoder(REQUETE_DE_L_ISSUE)),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let attendus: BTreeSet<String> = ["Allegro con brio", "Andante con moto", "Allegro", "Finale"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(titres(&corps), attendus, "{REQUETE_DE_L_ISSUE}");
    assert_eq!(corps["total"], 4);
    assert_eq!(corps["offset"], 0);
    assert_eq!(corps["has_more"], false);

    // La forme d'une piste est celle de la recherche de la bibliothèque : le
    // même seam (`joindre_dr_par_piste`), donc les mêmes clés.
    let (_, recherche) = appel(&app, "/api/v1/library/search?q=Finale").await;
    let de_la_recherche = recherche["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .find(|p| p["title"] == "Finale")
        .expect("Finale dans la recherche")
        .as_object()
        .unwrap()
        .clone();
    let de_la_requete = corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["title"] == "Finale")
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let cles = |o: &serde_json::Map<String, Value>| o.keys().cloned().collect::<BTreeSet<_>>();
    assert_eq!(cles(&de_la_requete), cles(&de_la_recherche));
    assert!(de_la_requete.contains_key("path_terms"));
    assert_eq!(de_la_requete["album_title"], de_la_recherche["album_title"]);

    // Une valeur piégée reste une valeur : aucune piste, pas d'erreur.
    let (statut, corps) = appel(
        &app,
        &format!(
            "/api/v1/library/query?q={}",
            encoder("title:\"x'; DROP TABLE tracks; --\"")
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["total"], 0);
    let (_, corps) = appel(&app, "/api/v1/library/query?q=composer%3Amahler").await;
    assert_eq!(titres(&corps), BTreeSet::from(["Adagio".to_string()]));
}

#[tokio::test]
async fn une_requete_mal_formee_rend_400_avec_sa_position_7872() {
    let app = banc();
    // (requête, position attendue)
    for (q, position) in [
        // Parenthèse jamais fermée : l'erreur est en fin de texte.
        ("composer:beethoven (conductor:karajan", 37),
        // Champ inconnu : l'erreur pointe le champ.
        ("year:1963 ratng:5", 10),
        // Requête vide.
        ("", 0),
    ] {
        let (statut, corps) = appel(&app, &format!("/api/v1/library/query?q={}", encoder(q))).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{q:?} : {corps}");
        assert_eq!(corps["position"], position, "{q:?} : {corps}");
        assert!(
            corps["attendu"].as_str().is_some_and(|a| !a.is_empty()),
            "{q:?} : attendu manquant : {corps}"
        );
        assert!(
            corps["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{q:?} : message manquant : {corps}"
        );
        assert_eq!(corps["code"], "requete_invalide");
    }
    // Sans `q` du tout : le même refus, pas un rejet d'extracteur en texte.
    let (statut, corps) = appel(&app, "/api/v1/library/query").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["position"], 0);
}

#[tokio::test]
async fn la_requete_se_pagine_7872() {
    let app = banc();
    // Six pistes de Karajan.
    let q = encoder("conductor:karajan");
    let mut vus = Vec::new();
    for (decalage, rendues, encore) in [(0, 2, true), (2, 2, true), (4, 2, false)] {
        let (statut, corps) = appel(
            &app,
            &format!("/api/v1/library/query?q={q}&limit=2&offset={decalage}"),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["total"], 6, "offset {decalage}");
        assert_eq!(corps["limit"], 2);
        assert_eq!(corps["offset"], decalage);
        assert_eq!(corps["has_more"], encore, "offset {decalage}");
        let items = corps["items"].as_array().unwrap();
        assert_eq!(items.len(), rendues, "offset {decalage}");
        vus.extend(items.iter().map(|p| p["id"].as_i64().unwrap()));
    }
    let distincts: BTreeSet<i64> = vus.iter().copied().collect();
    assert_eq!(distincts.len(), 6, "les pages se recouvrent : {vus:?}");
    // Au-delà de la fin : page vide, total inchangé.
    let (_, corps) = appel(
        &app,
        &format!("/api/v1/library/query?q={q}&limit=2&offset=10"),
    )
    .await;
    assert_eq!(corps["items"].as_array().unwrap().len(), 0);
    assert_eq!(corps["total"], 6);
    assert_eq!(corps["has_more"], false);
}
