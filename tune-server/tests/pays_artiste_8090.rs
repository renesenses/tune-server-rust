//! #8090 — la carte et le filtre Pays de la Bibliothèque lisent le pays de
//! l'ARTISTE (MusicBrainz), au choix avec le pays d'ÉDITION, et comptent des
//! ALBUMS.
//!
//! Le VRAI routeur, sur SQLite en mémoire et, quand `TUNE_TEST_PG_URL` est
//! posée, sur un VRAI PostgreSQL (`pg_8090`, déclarée dans
//! `test-postgres.yml`). Toute la bibliothèque d'épreuve vit sous un dossier
//! à elle, et chaque requête porte `folder=` : la base PostgreSQL partagée
//! avec les autres épreuves ne fausse aucun compte.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const MARQUE: &str = "pays-8090";
const DOSSIER: &str = "/m/pays-8090";

async fn get(app: &axum::Router, path: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{path} : {json}");
    json
}

fn menage(state: &AppState) {
    for sql in [
        format!(
            "DELETE FROM track_metadata WHERE track_id IN \
             (SELECT id FROM tracks WHERE file_path LIKE '{DOSSIER}/%')"
        ),
        format!("DELETE FROM tracks WHERE file_path LIKE '{DOSSIER}/%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        state
            .backend
            .execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

/// | album | artiste principal | pays (lu de)          | pistes | RELEASECOUNTRY |
/// |-------|-------------------|-----------------------|--------|----------------|
/// | A1    | Brel              | BE (MBID courant)     | 3      | FR sur les 3   |
/// | A2    | Gainsbourg        | FR (MBID courant)     | 2      | —              |
/// | A3    | Gainsbourg        | FR                    | 1 (piste de Brel) | —   |
/// | A4    | Piaf              | FR lu d'un ANCIEN MBID| 1      | —              |
/// | A5    | Divers            | '' (aucun)            | 1      | —              |
fn bibliotheque(state: &AppState) {
    menage(state);
    let b = &state.backend;
    let artiste = |nom: &str, mbid: &str, pays: Option<&str>, lu_de: Option<&str>| {
        let nom = format!("{MARQUE} {nom}");
        b.execute_returning_id(
            "INSERT INTO artists (name, musicbrainz_id, country, country_mbid) VALUES (?, ?, ?, ?)",
            &[
                &nom as &dyn ToSqlValue,
                &mbid as &dyn ToSqlValue,
                &pays as &dyn ToSqlValue,
                &lu_de as &dyn ToSqlValue,
            ],
        )
        .expect("artiste")
    };
    let brel = artiste("Brel", "MB-BREL-8090", Some("BE"), Some("mb-brel-8090"));
    let gainsbourg = artiste(
        "Gainsbourg",
        "mb-gainsbourg-8090",
        Some("fr"),
        Some("mb-gainsbourg-8090"),
    );
    let piaf = artiste(
        "Piaf",
        "mb-piaf-8090-nouveau",
        Some("FR"),
        Some("mb-piaf-8090-ancien"),
    );
    let divers = artiste("Divers", "mb-divers-8090", Some(""), Some("mb-divers-8090"));
    let album = |titre: &str, artiste: i64| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
            &[&titre as &dyn ToSqlValue, &artiste as &dyn ToSqlValue],
        )
        .expect("album")
    };
    let mut n = 0;
    let mut piste = |album: i64, artiste: i64| {
        n += 1;
        let chemin = format!("{DOSSIER}/{n}.flac");
        let titre = format!("{MARQUE} {n}");
        b.execute_returning_id(
            "INSERT INTO tracks (title, album_id, artist_id, file_path, duration_ms, format, source) \
             VALUES (?, ?, ?, ?, 1000, 'flac', 'local')",
            &[
                &titre as &dyn ToSqlValue,
                &album as &dyn ToSqlValue,
                &artiste as &dyn ToSqlValue,
                &chemin as &dyn ToSqlValue,
            ],
        )
        .expect("piste")
    };
    let a1 = album("A1", brel);
    let a2 = album("A2", gainsbourg);
    let a3 = album("A3", gainsbourg);
    let a4 = album("A4", piaf);
    let a5 = album("A5", divers);
    let mut edition = Vec::new();
    for _ in 0..3 {
        edition.push(piste(a1, brel));
    }
    piste(a2, gainsbourg);
    piste(a2, gainsbourg);
    piste(a3, brel);
    piste(a4, piaf);
    piste(a5, divers);
    for id in edition {
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'release_country', 'FR')",
            &[&id as &dyn ToSqlValue],
        )
        .expect("release_country");
    }
}

fn valeurs(facette: &Value) -> Vec<(String, i64)> {
    facette
        .as_array()
        .unwrap_or_else(|| panic!("une facette : {facette}"))
        .iter()
        .map(|v| {
            (
                v["value"].as_str().unwrap_or_default().to_string(),
                v["count"].as_i64().unwrap_or(-1),
            )
        })
        .collect()
}

async fn scenario(state: AppState) {
    bibliotheque(&state);
    let app = tune_server::routes::router(state.clone());
    let dossier = format!("folder={DOSSIER}");

    // La liste des pays : pays de l'ARTISTE PRINCIPAL, compté en ALBUMS.
    // Piaf (pays lu d'un ancien MBID) et « Divers » (aucun pays) n'y sont pas.
    let f = get(
        &app,
        &format!("/api/v1/library/facets?fields=artist_country,country_albums,country&{dossier}"),
    )
    .await;
    assert_eq!(
        valeurs(&f["artist_country"]),
        vec![("FR".to_string(), 2), ("BE".to_string(), 1)],
        "pays de l'artiste, en albums : {f}"
    );
    // Le pays d'édition : 3 TITRES, mais 1 ALBUM.
    assert_eq!(valeurs(&f["country"]), vec![("FR".to_string(), 3)], "{f}");
    assert_eq!(
        valeurs(&f["country_albums"]),
        vec![("FR".to_string(), 1)],
        "pays d'édition, en albums : {f}"
    );

    // Le filtre : les pistes des albums dont l'artiste principal est
    // français — la piste de Brel sur l'album de Gainsbourg comprise, celle
    // de Piaf (pays caduc) exclue. Sans la casse.
    for code in ["FR", "fr"] {
        let l = get(
            &app,
            &format!("/api/v1/library/tracks?limit=100&artist_country={code}&{dossier}"),
        )
        .await;
        assert_eq!(l["total"].as_i64(), Some(3), "artist_country={code} : {l}");
    }
    let l = get(
        &app,
        &format!("/api/v1/library/tracks?limit=100&artist_country=FR&artist_country=BE&{dossier}"),
    )
    .await;
    assert_eq!(l["total"].as_i64(), Some(6), "FR ou BE : {l}");
    let cartes = get(
        &app,
        &format!("/api/v1/library/albums-detailed?artist_country=FR&{dossier}"),
    )
    .await;
    assert_eq!(
        cartes["items"].as_array().map(Vec::len),
        Some(2),
        "deux albums d'artiste français : {cartes}"
    );

    // La facette ne se filtre pas elle-même ; les autres, si.
    let f = get(
        &app,
        &format!("/api/v1/library/facets?fields=artist_country,country_albums&artist_country=FR&{dossier}"),
    )
    .await;
    assert_eq!(
        valeurs(&f["artist_country"]),
        vec![("FR".to_string(), 2), ("BE".to_string(), 1)],
        "{f}"
    );
    assert_eq!(
        valeurs(&f["country_albums"]),
        Vec::<(String, i64)>::new(),
        "{f}"
    );
    let f = get(
        &app,
        &format!(
            "/api/v1/library/facets?fields=artist_country,country_albums&country=FR&{dossier}"
        ),
    )
    .await;
    assert_eq!(
        valeurs(&f["artist_country"]),
        vec![("BE".to_string(), 1)],
        "{f}"
    );
    assert_eq!(
        valeurs(&f["country_albums"]),
        vec![("FR".to_string(), 1)],
        "{f}"
    );
    menage(&state);
}

#[tokio::test(flavor = "multi_thread")]
async fn pays_de_l_artiste_en_albums_sqlite_8090() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    scenario(state).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8090_pays_de_l_artiste_en_albums() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    scenario(state).await;
}
