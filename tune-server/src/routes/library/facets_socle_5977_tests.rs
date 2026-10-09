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

/// #5993 — le rail compte PAR SOUSTRACTION (jeu sans les replis, moins les
/// seules pistes repliées). Il doit rendre, facette par facette, les MÊMES
/// effectifs que le socle complet posé dans chaque requête — avec et sans
/// plafond de valeurs, avec et sans facette cochée.
#[test]
fn le_rail_par_soustraction_compte_comme_le_socle_complet() {
    use super::{FacetQuery, SocleResolu, compter_avec_le_socle, compter_les_facettes};
    use std::collections::BTreeMap;
    let (_app, state) = bibliotheque();
    // Des valeurs dont l'effectif tombe à zéro une fois les replis retirés
    // (le format `mp3`, le label de la jumelle distante) et des casses mêlées.
    state
        .backend
        .execute_batch(
            "UPDATE tracks SET label = 'Distant Label' WHERE source = 'upnp';\n\
             UPDATE tracks SET label = 'Northern' WHERE album_id = 10 AND format = 'flac';\n\
             UPDATE tracks SET label = 'NORTHERN' WHERE album_id = 10 AND format = 'mp3';\n\
             UPDATE tracks SET composer = 'X' WHERE album_id = 13;",
        )
        .unwrap();
    const CHAMPS: &str = "genre,label,year,artist,format,sample_rate,bit_depth,composer,\
                          country,mood,source,rating,original_year,dr,instrument,favorite,\
                          playlist,untagged";
    let par_valeur = |v: &Value| -> BTreeMap<String, BTreeMap<String, i64>> {
        v.as_object()
            .unwrap()
            .iter()
            .map(|(champ, entrees)| {
                let m = entrees
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| {
                        (
                            e["value"].as_str().unwrap().to_lowercase(),
                            e["count"].as_i64().unwrap(),
                        )
                    })
                    .collect();
                (champ.clone(), m)
            })
            .collect()
    };
    for (limite, brut) in [
        (200, String::new()),
        (1, String::new()),
        (200, format!("genre={}", GENRE.replace(' ', "%20"))),
    ] {
        let q = || {
            FacetQuery {
                fields: Some(CHAMPS.to_string()),
                limit: Some(limite),
                ..Default::default()
            }
            .hydrate(Some(&brut))
            .ok()
            .expect("requête")
        };
        let par_soustraction = compter_les_facettes(&state, q());
        let socle_complet = compter_avec_le_socle(&state, q(), &SocleResolu::EnSql);
        assert_eq!(
            par_valeur(&par_soustraction),
            par_valeur(&socle_complet),
            "{brut}"
        );
        // Le rail par soustraction écarte bien quelque chose ici : sans
        // replis, `format` compterait aussi le mp3.
        assert!(
            par_soustraction["format"].as_array().unwrap().len() == 1,
            "{brut}"
        );
    }
}

/// #5993 — les cartes d'albums (correction par album) et la facette Dossiers
/// (soustraction) rendent EXACTEMENT ce que rend le socle complet posé dans
/// chaque requête : même JSON, pour plusieurs sélections, pages et dossiers.
#[test]
fn les_cartes_et_les_dossiers_sans_sonde_comptent_comme_le_socle_complet() {
    use super::super::albums_detailed::lire_les_cartes_sur;
    use super::super::folder_facet::{FolderPathQuery, lire_les_dossiers, lire_les_dossiers_sur};
    use super::{FacetQuery, SocleResolu};
    let (_app, state) = bibliotheque();
    let resolu = SocleResolu::resoudre(&state);
    assert!(
        matches!(&resolu, SocleResolu::Ecartees(ids) if !ids.is_empty()),
        "la fixture doit replier des pistes"
    );
    for (limite, decalage, brut) in [
        (500, 0, String::new()),
        (1, 1, String::new()),
        (500, 0, format!("genre={}", GENRE.replace(' ', "%20"))),
        (500, 0, "format=mp3".to_string()),
        (500, 0, "format=flac".to_string()),
    ] {
        let q = || {
            FacetQuery {
                limit: Some(limite),
                offset: Some(decalage),
                ..Default::default()
            }
            .hydrate(Some(&brut))
            .ok()
            .expect("requête")
        };
        assert_eq!(
            lire_les_cartes_sur(&state, q(), &resolu),
            lire_les_cartes_sur(&state, q(), &SocleResolu::EnSql),
            "cartes : {brut} limit={limite} offset={decalage}"
        );
        for chemin in [None, Some("/m"), Some("/m/Alda"), Some("/m/Masque")] {
            let p = || FolderPathQuery {
                path: chemin.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(
                lire_les_dossiers_sur(&state, q(), p(), &resolu),
                lire_les_dossiers_sur(&state, q(), p(), &SocleResolu::EnSql),
                "dossiers : {brut} {chemin:?}"
            );
            // Le socle limité aux albums du dossier (#5993) : même réponse.
            assert_eq!(
                lire_les_dossiers(&state, q(), p()),
                lire_les_dossiers_sur(&state, q(), p(), &SocleResolu::EnSql),
                "dossiers, socle du dossier : {brut} {chemin:?}"
            );
        }
    }
}

/// #5993 — le socle est gardé en mémoire sous le jeton de la base ; une
/// écriture le périme. Sur une base FICHIER (en mémoire, rien n'est gardé) :
/// une copie MP3 ajoutée se replie aussitôt, et redevient visible dès que son
/// original FLAC disparaît.
#[tokio::test]
async fn le_socle_garde_en_memoire_suit_chaque_ecriture() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("cache.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    assert!(
        state.backend.jeton_des_donnees().is_some(),
        "une base fichier SQLite doit rendre un jeton"
    );
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'A');\n\
             INSERT INTO albums (id, title, artist_id, source) VALUES (10, 'Disque', 1, 'local');\n\
             INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, file_path, \
               duration_ms, format, sample_rate, bit_depth, source) VALUES \
               (1, 'Un', 10, 1, 1, 1, '/m/A/Disque/01.flac', 1000, 'flac', 44100, 16, 'local'), \
               (2, 'Deux', 10, 1, 1, 2, '/m/A/Disque/02.flac', 1000, 'flac', 44100, 16, 'local');",
        )
        .unwrap();
    let app = crate::routes::router(state.clone());
    let formats = |rail: &Value| -> Vec<(String, i64)> {
        let mut v: Vec<(String, i64)> = rail["format"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["value"].as_str().unwrap().to_string(),
                    e["count"].as_i64().unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let uri = "/api/v1/library/facets?fields=format";
    assert_eq!(formats(&get(&app, uri).await), [("flac".to_string(), 2)]);
    // Une copie MP3 de la piste 1 : repliée.
    state
        .backend
        .execute_batch(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, file_path, \
               duration_ms, format, sample_rate, bit_depth, source) VALUES \
               (3, 'Un', 10, 1, 1, 1, '/m/A/Disque/01.mp3', 1000, 'mp3', 44100, NULL, 'local');",
        )
        .unwrap();
    assert_eq!(
        formats(&get(&app, uri).await),
        [("flac".to_string(), 2)],
        "la copie MP3 ajoutée doit être repliée"
    );
    let cartes = get(&app, "/api/v1/library/albums-detailed").await;
    assert_eq!(cartes["items"][0]["track_count"], 2, "{cartes}");
    // L'original disparaît : la copie redevient la piste visible.
    state
        .backend
        .execute_batch("DELETE FROM tracks WHERE id = 1;")
        .unwrap();
    assert_eq!(
        formats(&get(&app, uri).await),
        [("flac".to_string(), 1), ("mp3".to_string(), 1)],
        "le socle gardé en mémoire ne doit pas survivre à l'écriture"
    );
    let cartes = get(&app, "/api/v1/library/albums-detailed").await;
    assert_eq!(cartes["items"][0]["track_count"], 2, "{cartes}");
}
