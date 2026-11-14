//! « Joue avec » (`played_with`) de la page artiste sur une VRAIE base
//! PostgreSQL (#7768).
//!
//! Jumeau du témoin SQLite `tests/page_artiste_joue_avec_7768.rs`, réduit à
//! ce que PostgreSQL peut contredire : `track_credits.artist_id` lu en entier
//! ou en texte, la garde `CAST(tc.artist_id AS TEXT) <> ?`, les trois
//! branches `UNION` de ses pistes (fiche, nom, MBID), la fiche d'un crédit
//! retrouvée par `LOWER(name)` (pas de `COLLATE NOCASE` sur PostgreSQL), et
//! le vrai routeur au bout. L'`EXPLAIN` de la requête des crédits audibles
//! est imprimé (`--nocapture`), sans assertion sur le plan.
//!
//! Même doctrine que `pg_page_artiste_7767.rs` : variable ABSENTE ⇒ saut
//! annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::metadata::credits_joue_avec::joue_avec;
use tune_server::state::AppState;

/// Suffixe propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide rien et on ne compte que nos lignes.
const MARQUE: &str = "joue-avec-7768";

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

fn menage(b: &dyn DbBackend) {
    for sql in [
        format!(
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT id FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM tracks WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

fn artiste(b: &dyn DbBackend, nom: &str, mbid: Option<&str>) -> i64 {
    let nom = format!("{MARQUE} {nom}");
    b.execute_returning_id(
        "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
        &[&nom as &dyn ToSqlValue, &mbid as &dyn ToSqlValue],
    )
    .unwrap_or_else(|e| panic!("artiste {nom} : {e}"))
}

fn album(b: &dyn DbBackend, titre: &str, artiste: i64, annee: i64) -> i64 {
    let titre = format!("{MARQUE} {titre}");
    b.execute_returning_id(
        "INSERT INTO albums (title, artist_id, year, source, release_type) VALUES (?, ?, ?, 'local', 'album')",
        &[
            &titre as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
            &annee as &dyn ToSqlValue,
        ],
    )
    .unwrap_or_else(|e| panic!("album {titre} : {e}"))
}

fn pistes(b: &dyn DbBackend, album: i64, artiste: i64, n: i64) -> Vec<i64> {
    (1..=n)
        .map(|i| {
            let titre = format!("{MARQUE} piste {album}-{i}");
            b.execute_returning_id(
                "INSERT INTO tracks (title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES (?, ?, ?, ?, 200000, 'flac', 1, ?)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &titre as &dyn ToSqlValue,
                    &i as &dyn ToSqlValue,
                ],
            )
            .unwrap_or_else(|e| panic!("piste {titre} : {e}"))
        })
        .collect()
}

fn credit(
    b: &dyn DbBackend,
    piste: i64,
    artiste: Option<i64>,
    nom: &str,
    mbid: Option<&str>,
    role: &str,
    instrument: Option<&str>,
) {
    // Un nom commençant par `=` est pris tel quel (« Various Artists »).
    let nom = match nom.strip_prefix('=') {
        Some(brut) => brut.to_string(),
        None => format!("{MARQUE} {nom}"),
    };
    b.execute(
        "INSERT INTO track_credits (track_id, artist_id, artist_name, artist_mbid, role, instrument, position, source) \
         VALUES (?, ?, ?, ?, ?, ?, 0, 'musicbrainz')",
        &[
            &piste as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
            &nom as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &role as &dyn ToSqlValue,
            &instrument as &dyn ToSqlValue,
        ],
    )
    .unwrap_or_else(|e| panic!("crédit {nom} / {role} : {e}"));
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

const MBID_ILSE: &str = "0c7a9d4e-7768-4c1a-9b2e-000000000412";

fn resume(v: &Value) -> Vec<(bool, String, i64)> {
    v.as_array()
        .unwrap_or_else(|| panic!("played_with absente : {v}"))
        .iter()
        .map(|j| {
            (
                j["artist_id"].is_i64(),
                j["artist_name"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches(MARQUE)
                    .trim()
                    .to_string(),
                j["shared_tracks"].as_i64().unwrap_or(-1),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_7768_joue_avec_de_la_page_artiste() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b: &dyn DbBackend = state.backend.as_ref();
    menage(b);

    let ilse = artiste(b, "Ilse Brandt", Some(MBID_ILSE));
    let nora = artiste(b, "Nora Vidal", None);
    let sam = artiste(b, "Sam Okafor", None);
    let lena = artiste(b, "Lena Park", None);
    let okapi = artiste(b, "DJ Okapi", None);

    // Album de Nora : Ilse audible par sa fiche (1re piste), par son MBID
    // sous un autre nom (2e), par son nom seul (3e).
    let session = album(b, "Session", nora, 2018);
    let p = pistes(b, session, nora, 3);
    credit(
        b,
        p[0],
        Some(ilse),
        "Ilse Brandt",
        None,
        "performer",
        Some("bass"),
    );
    credit(
        b,
        p[1],
        None,
        "I. Brandt",
        Some(MBID_ILSE),
        "performer",
        Some("bass"),
    );
    credit(b, p[2], None, "Ilse Brandt", None, "vocal", None);
    for t in &p {
        // Compositrice partagée : ne compte pas.
        credit(b, *t, Some(lena), "Lena Park", None, "composer", None);
        // « Various Artists » en crédit : jamais.
        credit(b, *t, None, "=Various Artists", None, "artist", None);
    }
    // Sam Okafor crédité SANS fiche, en minuscules : la fiche est retrouvée
    // par `LOWER(name)`.
    for t in &p[..2] {
        credit(
            b,
            *t,
            None,
            "sam okafor",
            None,
            "performer",
            Some("saxophone"),
        );
    }
    // Remixeuse d'une piste de DJ Okapi.
    let rework = album(b, "Rework", okapi, 2024);
    let p = pistes(b, rework, okapi, 1);
    credit(b, p[0], Some(ilse), "Ilse Brandt", None, "remixer", None);
    // Lena : Ilse n'y fait qu'écrire.
    let sings = album(b, "Sings", lena, 2023);
    for t in pistes(b, sings, lena, 2) {
        credit(b, t, Some(ilse), "Ilse Brandt", None, "composer", None);
    }

    let attendu = vec![
        (true, "Nora Vidal".to_string(), 3),
        (true, "Sam Okafor".to_string(), 2),
        (true, "DJ Okapi".to_string(), 1),
    ];
    let lu = joue_avec(&state.backend, ilse);
    assert_eq!(
        lu.iter().map(|j| j.artist_id).collect::<Vec<_>>(),
        vec![Some(nora), Some(sam), Some(okapi)],
        "{lu:?}"
    );

    let app = tune_server::routes::router(state.clone());
    let (status, body) = get(
        &app,
        &format!("/api/v1/library/artists/{ilse}/albums?sections=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(resume(&body["played_with"]), attendu, "{body}");

    let plan = b
        .query_many(
            "EXPLAIN SELECT tc.track_id, tc.artist_id, tc.artist_name, tc.artist_mbid              FROM track_credits tc JOIN tracks t ON t.id = tc.track_id              JOIN albums a ON a.id = t.album_id              WHERE tc.track_id IN (SELECT id FROM tracks WHERE artist_id = ?)                AND tc.role IN ('artist', 'performer', 'vocal', 'conductor', 'remixer')                AND (tc.artist_id IS NULL OR CAST(tc.artist_id AS TEXT) <> ?)",
            &[&ilse as &dyn ToSqlValue, &ilse.to_string() as &dyn ToSqlValue],
        )
        .expect("EXPLAIN des crédits audibles");
    eprintln!("EXPLAIN joue avec (#7768) :");
    for r in plan {
        eprintln!(
            "  {}",
            r.first().and_then(|v| v.as_string()).unwrap_or_default()
        );
    }

    menage(b);
}

/// #2961 (web) — jumeau PostgreSQL du duo du .18 : album servi deux fois
/// (local et copie UPnP, même titre), personnel d'album recopié sur 4 des 5
/// pistes, membres du duo crédités. Rend des enregistrements (pas 8 pistes),
/// `album_wide`, et ni « A » ni « B » dans « joue avec » de « A & B ».
#[tokio::test(flavor = "multi_thread")]
async fn pg_2961_ni_membres_du_duo_ni_copies_comptees_deux_fois() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b: &dyn DbBackend = state.backend.as_ref();
    menage(b);

    // « joue-avec-7768 DjeuhDjoah & Lieutenant Nicholson » : parties
    // « joue-avec-7768 DjeuhDjoah » et « Lieutenant Nicholson ».
    let duo = artiste(b, "DjeuhDjoah & Lieutenant Nicholson", None);
    for source in ["local", "upnp"] {
        let a = album(b, "Aimez ces airs", duo, 2021);
        b.execute(
            "UPDATE albums SET source = ? WHERE id = ?",
            &[&source as &dyn ToSqlValue, &a as &dyn ToSqlValue],
        )
        .expect("source de l'album");
        for i in 1..=5i64 {
            let titre = format!("{MARQUE} titre {i}");
            let chemin = format!("/m/{source}/{i}.flac");
            let t = b
                .execute_returning_id(
                    "INSERT INTO tracks (title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                     VALUES (?, ?, ?, ?, 200000, 'flac', 1, ?)",
                    &[
                        &titre as &dyn ToSqlValue,
                        &a as &dyn ToSqlValue,
                        &duo as &dyn ToSqlValue,
                        &chemin as &dyn ToSqlValue,
                        &i as &dyn ToSqlValue,
                    ],
                )
                .unwrap_or_else(|e| panic!("piste {titre} : {e}"));
            if i <= 4 {
                credit(b, t, None, "Alexis Valet", None, "performer", None);
                credit(b, t, None, "Djeuhdjoah", None, "performer", None);
                credit(b, t, None, "=Lieutenant Nicholson", None, "performer", None);
            } else {
                credit(b, t, None, "Ben Invité", None, "performer", None);
            }
        }
    }

    let lu = joue_avec(&state.backend, duo);
    let resume_2961: Vec<(String, i64, i64, bool)> = lu
        .iter()
        .map(|j| {
            (
                j.artist_name.trim_start_matches(MARQUE).trim().to_string(),
                j.shared_tracks,
                j.shared_albums,
                j.album_wide,
            )
        })
        .collect();
    assert_eq!(
        resume_2961,
        vec![
            ("Alexis Valet".to_string(), 4, 1, true),
            ("Ben Invité".to_string(), 1, 1, false),
        ],
        "{lu:?}"
    );

    let app = tune_server::routes::router(state.clone());
    let (status, body) = get(
        &app,
        &format!("/api/v1/library/artists/{duo}/albums?sections=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["played_with"][0]["shared_albums"], 1, "{body}");
    assert_eq!(body["played_with"][0]["album_wide"], true, "{body}");

    menage(b);
}
