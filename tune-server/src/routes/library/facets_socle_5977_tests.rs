//! #5977 — Oxygen, facettes : « le nombre total de pistes, quelle que soit la
//! facette, est systématiquement doublé ; dans le bandeau, le total est
//! juste » (Dominique Pamingle, fil 2180, 08/10/2026).
//!
//! Le bandeau lit `total` de `GET /library/tracks`, dont le socle porte TROIS
//! prédicats (albums masqués #1391, double distant #4146, copie de moindre
//! qualité #4101). Le rail (`GET /library/facets`, `build_conditions`) n'en
//! posait qu'UN. Le témoin de #1864 comparait les deux SANS le socle : sa
//! fixture n'avait ni copie, ni album distant, ni album masqué.
//!
//! Ici, la fixture porte les trois cas, et l'on compare le rail, les cartes
//! d'`albums-detailed` et la facette Dossiers — qui partagent
//! `build_conditions` — à la liste, à travers le VRAI routeur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::state::AppState;

const GENRE: &str = "Atmospheric Black Metal";

/// *A Distant Fire* d'Alda (US) : six pistes FLAC locales, plus
/// - trois copies MP3 de moindre qualité, même album, même disque, même
///   numéro, même titre (#4101) ;
/// - le même album servi par un serveur UPnP, six pistes (#4146) ;
/// - un album masqué de deux pistes (#1391) ;
/// - un album de jazz de deux pistes, qui ne replie rien.
///
/// La liste rend donc 6 + 2 = 8 pistes ; sans socle, le rail en compterait
/// 6 + 3 + 6 + 2 + 2 = 19, et 15 pour le genre.
fn bibliotheque() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let mut sql = String::from(
        "INSERT INTO artists (id, name) VALUES (1, 'Alda (US)'), (2, 'Masqué'), (3, 'Jazzman');\n\
         INSERT INTO albums (id, title, artist_id, source) VALUES \
           (10, 'A Distant Fire', 1, 'local'), \
           (11, 'A Distant Fire', 1, 'upnp'), \
           (12, 'Caché', 2, 'local'), \
           (13, 'Autre', 3, 'local');\n\
         INSERT INTO hidden_items (profile_id, item_type, item_id) VALUES (1, 'album', 12);\n",
    );
    let mut id = 100;
    let mut piste = |sql: &mut String,
                     album: i64,
                     artiste: i64,
                     n: i64,
                     chemin: &str,
                     format: &str,
                     bits: &str,
                     source: &str,
                     genre: &str| {
        id += 1;
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, duration_ms, format, sample_rate, bit_depth, source, genre, year) \
             VALUES ({id}, 'Titre {n}', {album}, {artiste}, 1, {n}, '{chemin}', 300000, \
             '{format}', 44100, {bits}, '{source}', '{genre}', 2021);\n"
        ));
    };
    for n in 1..=6 {
        piste(
            &mut sql,
            10,
            1,
            n,
            &format!("/m/Alda/A Distant Fire/{n:02}.flac"),
            "flac",
            "16",
            "local",
            GENRE,
        );
    }
    for n in 1..=3 {
        piste(
            &mut sql,
            10,
            1,
            n,
            &format!("/m/Alda/A Distant Fire/{n:02}.mp3"),
            "mp3",
            "NULL",
            "local",
            GENRE,
        );
    }
    for n in 1..=6 {
        piste(
            &mut sql,
            11,
            1,
            n,
            &format!("http://nas:9000/alda/{n:02}.flac"),
            "flac",
            "16",
            "upnp",
            GENRE,
        );
    }
    for n in 1..=2 {
        piste(
            &mut sql,
            12,
            2,
            n,
            &format!("/m/Masque/Cache/{n:02}.flac"),
            "flac",
            "16",
            "local",
            GENRE,
        );
    }
    for n in 1..=2 {
        piste(
            &mut sql,
            13,
            3,
            n,
            &format!("/m/Jazzman/Autre/{n:02}.flac"),
            "flac",
            "24",
            "local",
            "Jazz",
        );
    }
    state.backend.execute_batch(&sql).expect("fixture");
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn get(app: &axum::Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK, "{chemin}");
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn effectif(rail: &Value, champ: &str, valeur: &str) -> i64 {
    rail[champ]
        .as_array()
        .unwrap_or_else(|| panic!("facette {champ} absente : {rail}"))
        .iter()
        .find(|e| e["value"] == valeur)
        .and_then(|e| e["count"].as_i64())
        .unwrap_or(0)
}

fn somme(rail: &Value, champ: &str) -> i64 {
    rail[champ]
        .as_array()
        .unwrap_or_else(|| panic!("facette {champ} absente : {rail}"))
        .iter()
        .filter_map(|e| e["count"].as_i64())
        .sum()
}

/// Le cas de Dominique : le genre coché, le rail et le bandeau doivent dire
/// le même nombre.
#[tokio::test]
async fn l_effectif_d_un_genre_vaut_le_total_de_la_liste_sous_ce_genre() {
    let (app, _s) = bibliotheque();
    let encode = urlencoding::encode(GENRE);
    let liste = get(
        &app,
        &format!("/api/v1/library/tracks?limit=100&genre={encode}"),
    )
    .await;
    assert_eq!(liste["total"], 6, "la liste replie le socle : {liste}");
    let rail = get(&app, "/api/v1/library/facets?fields=genre&limit=0").await;
    assert_eq!(
        effectif(&rail, "genre", GENRE),
        6,
        "le rail doit compter ce que la liste rend, pas le double : {rail}"
    );
}

/// La parité #1864, socle COMPRIS : pour chaque facette monovaluée et
/// toujours renseignée, la somme des effectifs vaut le total de la liste —
/// sans filtre, puis sous chaque valeur de genre.
#[tokio::test]
async fn la_somme_des_effectifs_vaut_le_total_de_la_liste_socle_compris() {
    let (app, _s) = bibliotheque();
    let liste = get(&app, "/api/v1/library/tracks?limit=100").await;
    assert_eq!(liste["total"], 8, "{liste}");
    let rail = get(
        &app,
        "/api/v1/library/facets?fields=format,sample_rate,year,artist,genre&limit=0",
    )
    .await;
    for champ in ["format", "sample_rate", "year", "artist", "genre"] {
        assert_eq!(
            somme(&rail, champ),
            8,
            "facette {champ} : le rail totalise autre chose que la liste : {rail}"
        );
    }
    for genre in [GENRE, "Jazz"] {
        let encode = urlencoding::encode(genre);
        let liste = get(
            &app,
            &format!("/api/v1/library/tracks?limit=100&genre={encode}"),
        )
        .await;
        let rail = get(
            &app,
            &format!("/api/v1/library/facets?fields=format&limit=0&genre={encode}"),
        )
        .await;
        assert_eq!(
            somme(&rail, "format"),
            liste["total"].as_i64().unwrap(),
            "sous genre={genre} : rail {rail} / liste {}",
            liste["total"]
        );
    }
}

/// `albums-detailed` et la facette Dossiers partagent `build_conditions` :
/// la carte d'*A Distant Fire* compte six pistes, sa jumelle UPnP n'a pas de
/// carte, et le dossier de l'album en annonce six.
#[tokio::test]
async fn les_cartes_d_albums_et_les_dossiers_comptent_comme_la_liste() {
    let (app, _s) = bibliotheque();
    let cartes = get(&app, "/api/v1/library/albums-detailed").await;
    let items = cartes["items"].as_array().expect("items");
    let ids: Vec<i64> = items
        .iter()
        .filter_map(|c| c["album_id"].as_i64())
        .collect();
    assert!(
        !ids.contains(&11),
        "la jumelle UPnP n'a pas de carte : {cartes}"
    );
    assert!(
        !ids.contains(&12),
        "l'album masqué n'a pas de carte : {cartes}"
    );
    let carte = items
        .iter()
        .find(|c| c["album_id"] == 10)
        .unwrap_or_else(|| panic!("carte d'A Distant Fire absente : {cartes}"));
    assert_eq!(carte["track_count"], 6, "{carte}");
    assert_eq!(cartes["total"], 2, "{cartes}");

    let dossiers = get(&app, "/api/v1/library/folder-facet?path=/m/Alda").await;
    let enfant = dossiers["children"]
        .as_array()
        .expect("children")
        .iter()
        .find(|c| c["name"] == "A Distant Fire")
        .cloned()
        .unwrap_or_else(|| panic!("dossier absent : {dossiers}"));
    assert_eq!(enfant["count"], 6, "{dossiers}");
}

/// Le socle RÉSOLU (une lecture, puis `t.id NOT IN (…)`) et le socle EN SQL
/// (les fragments de la liste, rejoués par chaque requête) comptent pareil :
/// le premier n'est qu'une mise en cache du second.
#[test]
fn le_socle_resolu_et_le_socle_en_sql_comptent_pareil() {
    use super::{FacetQuery, SocleResolu, build_conditions, genre_facet};
    let (_app, state) = bibliotheque();
    let engine = state.backend.engine();
    let resolu = SocleResolu::resoudre(&state);
    match &resolu {
        SocleResolu::Ecartees(ids) => assert_eq!(
            ids.len(),
            9,
            "trois copies MP3 et six pistes UPnP écartées : {ids:?}"
        ),
        SocleResolu::EnSql => panic!("le socle doit se résoudre sur SQLite"),
    }
    let q = FacetQuery::default();
    for socle in [resolu, SocleResolu::EnSql] {
        let (conds, params) = build_conditions(&q, engine, "genre", None, &socle);
        let rows = genre_facet(&state, None, &conds, &params);
        let compte = |g: &str| rows.iter().find(|(v, _)| v == g).map(|r| r.1);
        assert_eq!(compte(GENRE), Some(6), "{rows:?}");
        assert_eq!(compte("Jazz"), Some(2), "{rows:?}");
    }
}
