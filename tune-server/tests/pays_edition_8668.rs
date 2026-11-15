//! #8668 — la carte des pays de la Bibliothèque :
//!
//! 1. le pays d'ÉDITION lit la balise RELEASECOUNTRY, ou, pour un album sans
//!    balise, le pays lu sur sa release MusicBrainz (`albums.release_country`) —
//!    facette `country_albums`, filtre `country=`, cartes `albums-detailed` ;
//! 2. `GET /library/album-countries` rend le pays de chaque album, que le
//!    client croise avec les albums que sa grille garde (source, qualité…).
//!
//! Le VRAI routeur, sur SQLite en mémoire et, quand `TUNE_TEST_PG_URL` est
//! posée, sur un VRAI PostgreSQL (`pg_8668`, déclarée dans
//! `test-postgres.yml`). La bibliothèque d'épreuve vit sous un dossier à
//! elle ; `album-countries` (sans portée) est lu sur NOS identifiants.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const MARQUE: &str = "pays-8668";
const DOSSIER: &str = "/m/pays-8668";

async fn appel(app: &axum::Router, path: &str) -> (StatusCode, Value) {
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

async fn get(app: &axum::Router, path: &str) -> Value {
    let (status, json) = appel(app, path).await;
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

struct Ids {
    artiste: i64,
    caduc_artiste: i64,
    balise: i64,
    mb_jp: i64,
    caduc: i64,
    vide: i64,
}

/// | album  | balise RELEASECOUNTRY | release MB (lu de)          | pistes |
/// |--------|-----------------------|-----------------------------|--------|
/// | balise | FR sur les 2          | US (release courante)       | 2      |
/// | mb_jp  | —                     | jp (release courante)       | 2      |
/// | caduc  | —                     | DE lu d'une ANCIENNE release| 1      |
/// | vide   | —                     | '' (MusicBrainz : aucun)    | 1      |
///
/// Artiste principal de `balise` et `mb_jp` : français (MBID courant).
fn bibliotheque(state: &AppState) -> Ids {
    menage(state);
    let b = &state.backend;
    let nom = format!("{MARQUE} Artiste");
    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name, musicbrainz_id, country, country_mbid) \
             VALUES (?, 'mb-artiste-8668', 'FR', 'mb-artiste-8668')",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    // Pays lu d'un ANCIEN MBID : la fiche ne le montre pas.
    let nom_caduc = format!("{MARQUE} Caduc");
    let caduc_artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name, musicbrainz_id, country, country_mbid) \
             VALUES (?, 'mb-caduc-8668-neuf', 'DE', 'mb-caduc-8668-ancien')",
            &[&nom_caduc as &dyn ToSqlValue],
        )
        .expect("artiste caduc");
    let album = |titre: &str, release: &str, pays: &str, lu_de: &str| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id, musicbrainz_release_id, release_country, \
             release_country_mbid) VALUES (?, ?, ?, ?, ?)",
            &[
                &titre as &dyn ToSqlValue,
                &artiste as &dyn ToSqlValue,
                &release as &dyn ToSqlValue,
                &pays as &dyn ToSqlValue,
                &lu_de as &dyn ToSqlValue,
            ],
        )
        .expect("album")
    };
    let balise = album("balise", "r-balise-8668", "US", "r-balise-8668");
    let mb_jp = album("mb_jp", "R-JP-8668", "jp", "r-jp-8668");
    let caduc = album("caduc", "r-caduc-8668-neuve", "DE", "r-caduc-8668-ancienne");
    let vide = album("vide", "r-vide-8668", "", "r-vide-8668");
    let mut n = 0;
    let mut piste = |album: i64| {
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
    for _ in 0..2 {
        let id = piste(balise);
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'release_country', 'FR')",
            &[&id as &dyn ToSqlValue],
        )
        .expect("release_country");
    }
    piste(mb_jp);
    piste(mb_jp);
    piste(caduc);
    piste(vide);
    Ids {
        artiste,
        caduc_artiste,
        balise,
        mb_jp,
        caduc,
        vide,
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

/// Les paires `(album, pays)` de NOS albums.
fn paires(r: &Value, ids: &[i64]) -> Vec<(i64, String)> {
    let mut v: Vec<(i64, String)> = r["albums"]
        .as_array()
        .unwrap_or_else(|| panic!("albums : {r}"))
        .iter()
        .filter_map(|p| Some((p[0].as_i64()?, p[1].as_str()?.to_string())))
        .filter(|(id, _)| ids.contains(id))
        .collect();
    v.sort();
    v
}

async fn scenario(state: AppState) {
    let ids = bibliotheque(&state);
    let app = tune_server::routes::router(state.clone());
    let dossier = format!("folder={DOSSIER}");

    // Le pays d'édition, en ALBUMS : la balise prime (FR, pas US) ; l'album
    // sans balise compte sous le pays de sa release MusicBrainz ; un pays lu
    // d'une ancienne release, ou vide, ne compte pas.
    let f = get(
        &app,
        &format!("/api/v1/library/facets?fields=country_albums&{dossier}"),
    )
    .await;
    assert_eq!(
        valeurs(&f["country_albums"]),
        vec![("FR".to_string(), 1), ("JP".to_string(), 1)],
        "{f}"
    );

    // Le filtre `country=` suit la même règle, sans la casse pour le pays
    // MusicBrainz ; la balise reste comparée telle qu'écrite.
    for (code, attendu) in [("JP", 2), ("jp", 2), ("FR", 2), ("US", 0), ("DE", 0)] {
        let l = get(
            &app,
            &format!("/api/v1/library/tracks?limit=100&country={code}&{dossier}"),
        )
        .await;
        assert_eq!(l["total"].as_i64(), Some(attendu), "country={code} : {l}");
    }
    let l = get(
        &app,
        &format!("/api/v1/library/tracks?limit=100&country=FR&country=JP&{dossier}"),
    )
    .await;
    assert_eq!(l["total"].as_i64(), Some(4), "FR ou JP : {l}");
    let cartes = get(
        &app,
        &format!("/api/v1/library/albums-detailed?country=JP&{dossier}"),
    )
    .await;
    assert_eq!(
        cartes["items"].as_array().map(Vec::len),
        Some(1),
        "un album japonais : {cartes}"
    );
    // La facette du pays de l'artiste se resserre sur le pays d'édition.
    let f = get(
        &app,
        &format!("/api/v1/library/facets?fields=artist_country&country=JP&{dossier}"),
    )
    .await;
    assert_eq!(
        valeurs(&f["artist_country"]),
        vec![("FR".to_string(), 1)],
        "{f}"
    );

    // Le pays de chaque album, pour la carte qui suit les filtres de la grille.
    let tous = [ids.balise, ids.mb_jp, ids.caduc, ids.vide];
    let r = get(&app, "/api/v1/library/album-countries?source=release").await;
    assert_eq!(r["source"], "release", "{r}");
    assert_eq!(
        paires(&r, &tous),
        vec![
            (ids.balise, "FR".to_string()),
            (ids.mb_jp, "JP".to_string())
        ],
        "{r}"
    );
    for chemin in [
        "/api/v1/library/album-countries",
        "/api/v1/library/album-countries?source=artist",
    ] {
        let r = get(&app, chemin).await;
        assert_eq!(r["source"], "artist", "{r}");
        let mut attendu: Vec<(i64, String)> =
            tous.iter().map(|&id| (id, "FR".to_string())).collect();
        attendu.sort();
        assert_eq!(paires(&r, &tous), attendu, "{chemin} : {r}");
    }
    // La fiche artiste porte son pays (null s'il est caduc).
    let fiche = get(&app, &format!("/api/v1/library/artists/{}", ids.artiste)).await;
    assert_eq!(fiche["country"], "FR", "{fiche}");
    let fiche = get(
        &app,
        &format!("/api/v1/library/artists/{}", ids.caduc_artiste),
    )
    .await;
    assert!(fiche["country"].is_null(), "{fiche}");

    let (status, _) = appel(&app, "/api/v1/library/album-countries?source=label").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    menage(&state);
}

#[tokio::test(flavor = "multi_thread")]
async fn pays_d_edition_et_pays_par_album_sqlite_8668() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    scenario(state).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8668_pays_d_edition_et_pays_par_album() {
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
