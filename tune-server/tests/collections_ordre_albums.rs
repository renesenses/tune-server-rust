//! Ordre des albums dans un dossier « Collections » (#2675).
//!
//! `GET /library/collections/{id}/albums` rendait les albums dans l'ordre du
//! tableau `album_ids` — c'est-à-dire l'ordre d'AJOUT, jamais trié (Lulu/JLuc,
//! fil forum 1591 : « comment exécuter une remise en ordre alphabétique ? »).
//!
//! Le tri est fait en Rust, pas en SQL : le handler ne fait aucune requête
//! multi-lignes (il relit chaque album par `AlbumRepo::get(id)`, un `WHERE
//! a.id = ?` à la fois), donc il n'y a aucun `ORDER BY` où se raccrocher — et
//! un tri Rust rend le MÊME ordre sur SQLite et sur PostgreSQL, ce qu'un
//! `ORDER BY LOWER(...)` ne garantit pas (collations divergentes).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;

fn make_app_with_state() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Crée un album (et son artiste) et renvoie son id.
fn seed_album(state: &tune_server::state::AppState, artist: &str, title: &str) -> i64 {
    let artists = ArtistRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let a = artists.get_or_create(artist, None, None).unwrap();
    let album = albums
        .get_or_create(title, a.id.unwrap(), None)
        .unwrap_or_else(|e| panic!("album {title}: {e}"));
    album.id.unwrap()
}

/// Crée un dossier et y verse les albums DANS L'ORDRE DONNÉ, puis renvoie les
/// titres tels que l'endpoint les rend.
async fn titles_after_adding(
    app: &axum::Router,
    name: &str,
    album_ids: &[i64],
    query: &str,
) -> Vec<String> {
    let (st, col) = post_json(app, "/api/v1/library/collections", json!({"name": name})).await;
    assert_eq!(st, StatusCode::CREATED, "création du dossier: {col}");
    let cid = col["id"].as_i64().unwrap();
    for id in album_ids {
        let (st, _) = post_json(
            app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "ajout de l'album {id}");
    }
    let (st, body) = get(
        app,
        &format!("/api/v1/library/collections/{cid}/albums{query}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "lecture du dossier: {body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Renvoie les noms d'artiste rendus par l'endpoint.
async fn artists_after_adding(app: &axum::Router, name: &str, album_ids: &[i64]) -> Vec<String> {
    let (st, col) = post_json(app, "/api/v1/library/collections", json!({"name": name})).await;
    assert_eq!(st, StatusCode::CREATED, "création du dossier: {col}");
    let cid = col["id"].as_i64().unwrap();
    for id in album_ids {
        let (st, _) = post_json(
            app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "ajout de l'album {id}");
    }
    let (st, body) = get(app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    assert_eq!(st, StatusCode::OK, "lecture du dossier: {body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|a| a["artist_name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Le point même du ticket : l'ordre d'ajout ne doit plus décider de l'affichage.
#[tokio::test]
async fn albums_tries_par_artiste_et_non_par_ordre_d_ajout() {
    let (app, state) = make_app_with_state();
    let zappa = seed_album(&state, "Frank Zappa", "Hot Rats");
    let abba = seed_album(&state, "ABBA", "Arrival");
    let beethoven = seed_album(&state, "Beethoven", "Symphonies");

    // Ajoutés dans le désordre, exprès.
    let got = artists_after_adding(&app, "Mon coffret", &[zappa, abba, beethoven]).await;
    assert_eq!(got, vec!["ABBA", "Beethoven", "Frank Zappa"]);
}

/// `CD2` après `CD1` mais AVANT `CD10` — ce qu'un tri lexicographique rate.
#[tokio::test]
async fn ordre_naturel_des_numeros_de_disque() {
    let (app, state) = make_app_with_state();
    let cd10 = seed_album(&state, "Wagner", "Der Ring CD10");
    let cd1 = seed_album(&state, "Wagner", "Der Ring CD1");
    let cd2 = seed_album(&state, "Wagner", "Der Ring CD2");
    let cd9 = seed_album(&state, "Wagner", "Der Ring CD9");

    let got = titles_after_adding(&app, "Le Ring", &[cd10, cd1, cd2, cd9], "").await;
    assert_eq!(
        got,
        vec![
            "Der Ring CD1",
            "Der Ring CD2",
            "Der Ring CD9",
            "Der Ring CD10"
        ]
    );
}

/// Accents et casse : « eagles » < « Édith Piaf » < « Ella Fitzgerald ».
/// Sans repli d'accents, `É` (U+00C9) passerait après tout l'ASCII ; sans
/// repli de casse, `Ella` passerait avant `eagles`.
#[tokio::test]
async fn accents_et_casse_ne_derangent_pas_l_ordre() {
    let (app, state) = make_app_with_state();
    let ella = seed_album(&state, "Ella Fitzgerald", "Songbook");
    let edith = seed_album(&state, "Édith Piaf", "L'Hymne à l'amour");
    let eagles = seed_album(&state, "eagles", "Hotel California");

    let got = artists_after_adding(&app, "Voix", &[ella, edith, eagles]).await;
    assert_eq!(got, vec!["eagles", "Édith Piaf", "Ella Fitzgerald"]);
}

/// Un dossier à un seul album reste un dossier à un seul album.
#[tokio::test]
async fn dossier_a_un_seul_album() {
    let (app, state) = make_app_with_state();
    let solo = seed_album(&state, "Nina Simone", "Pastel Blues");
    let got = titles_after_adding(&app, "Solo", &[solo], "").await;
    assert_eq!(got, vec!["Pastel Blues"]);
}

/// L'ordre d'ajout reste joignable — une séquence d'écoute montée à la main
/// n'est pas perdue, elle est derrière `?sort=added`.
#[tokio::test]
async fn sort_added_rend_l_ordre_d_ajout() {
    let (app, state) = make_app_with_state();
    let zappa = seed_album(&state, "Frank Zappa", "Hot Rats");
    let abba = seed_album(&state, "ABBA", "Arrival");

    let got = titles_after_adding(&app, "Séquence", &[zappa, abba], "?sort=added").await;
    assert_eq!(got, vec!["Hot Rats", "Arrival"]);
}

/// `?sort=title` trie par titre d'album, comme la Bibliothèque en vue Albums.
#[tokio::test]
async fn sort_title_trie_par_titre_d_album() {
    let (app, state) = make_app_with_state();
    let z = seed_album(&state, "ABBA", "Zeppelin");
    let a = seed_album(&state, "Frank Zappa", "Apostrophe");

    let got = titles_after_adding(&app, "Par titre", &[z, a], "?sort=title").await;
    assert_eq!(got, vec!["Apostrophe", "Zeppelin"]);
}
