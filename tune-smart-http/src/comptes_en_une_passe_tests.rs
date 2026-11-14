//! Recette du 13/11/2026 (n° 73), puis du 14/11 — « Collections intelligentes
//! très, trop lent » sur le .18 (9 488 albums, 96 438 pistes).
//!
//! Deux remèdes, deux familles d'épreuves :
//!
//! 1. les deux comptes d'une collection (albums, pistes) viennent d'UNE
//!    requête ([`sql_comptes_de_collection`]) au lieu de deux : elle doit
//!    rendre EXACTEMENT ce que rendaient les deux anciennes
//!    ([`sql_compte_de_collection`]), sur SQLite et sur PostgreSQL, pour des
//!    règles de toutes les formes (« tout » / « au moins une », champs
//!    d'album et de piste, « vide », « ne contient pas », bornes, albums
//!    sans piste) ; et la LISTE des collections doit porter ces comptes ;
//! 2. les colonnes de piste que lisent les règles (format, fréquence,
//!    résolution, année, date d'ajout, répertoire) sont portées par un index
//!    à côté de l'album (`TRACKS_ALBUM_COLLECTIONS_INDEX`) : la jointure ne
//!    relit plus aucune ligne de la table des pistes.
//!
//! Contre-épreuve du second point : l'épreuve retire elle-même les deux index
//! et constate que le plan relit alors la table.

use std::sync::Arc;

use serde_json::Value;
use tune_core::db::backend::DbBackend;

use super::{
    build_album_query, sql_albums_de_collection, sql_compte_de_collection,
    sql_comptes_de_collection,
};
use crate::smart_refs::{DbRefResolver, RefCtx};

/// Des règles de toutes les formes que l'éditeur écrit, sur les champs de
/// COLONNE (ceux que les deux moteurs lisent dans `albums`, `artists` et
/// `tracks` seuls).
const REGLES: &[(&str, &str)] = &[
    ("all", r#"[]"#),
    (
        "all",
        r#"[{"field":"genre","operator":"contains","value":"jazz"}]"#,
    ),
    ("all", r#"[{"field":"genre","op":"=","value":"Jazz"}]"#),
    (
        "all",
        r#"[{"field":"format","operator":"equals","value":"dsd"}]"#,
    ),
    (
        "all",
        r#"[{"field":"sample_rate","operator":"greater_than","value":"96000"},{"field":"genre","op":"=","value":"Jazz"}]"#,
    ),
    (
        "all",
        r#"[{"field":"folder","op":"starts_with","value":"/m/b/"}]"#,
    ),
    ("all", r#"[{"field":"year","op":"=","value":2026}]"#),
    (
        "all",
        r#"[{"field":"added_at","operator":"greater_than","value":"90d"}]"#,
    ),
    (
        "all",
        r#"[{"field":"cover_path","operator":"is_empty","value":""}]"#,
    ),
    // Une colonne de PISTE « vide » : les albums SANS piste y répondent
    // (la jointure à gauche leur donne une piste NULL).
    (
        "all",
        r#"[{"field":"genre","operator":"is_empty","value":""}]"#,
    ),
    (
        "any",
        r#"[{"field":"genre","operator":"contains","value":"soul"},{"field":"format","operator":"equals","value":"dsd"}]"#,
    ),
    (
        "all",
        r#"[{"field":"genre","operator":"not_contains","value":"rock"}]"#,
    ),
    (
        "all",
        r#"[{"field":"bit_depth","op":"between","value":[16,24]}]"#,
    ),
    ("all", r#"[{"field":"duration","op":">=","value":300000}]"#),
    (
        "any",
        r#"[{"field":"folder","op":"ends_with","value":".dsf"},{"field":"year","op":"<","value":2000}]"#,
    ),
    ("all", r#"[{"field":"label","op":"=","value":"ECM"}]"#),
    (
        "all",
        r#"[{"field":"artist","op":"=","value":"Artiste 3"}]"#,
    ),
    (
        "all",
        r#"[{"field":"label","op":"=","value":"ECM"},{"field":"format","op":"!=","value":"flac"}]"#,
    ),
];

/// Le jeu d'essai, en SQL que les deux moteurs acceptent : 12 artistes,
/// 200 albums dont les 10 derniers SANS piste, et des colonnes de piste
/// tantôt remplies, tantôt NULL ou vides. `suffixe` distingue les titres
/// (base PostgreSQL partagée) ; `base` décale les identifiants.
fn jeu_d_essai(base: i64, suffixe: &str) -> Vec<String> {
    let maintenant = chrono::Utc::now().timestamp();
    let genres = [
        "'Jazz'",
        "'Rock'",
        "'Soul'",
        "'Pop Rock'",
        "NULL",
        "''",
        "'Classical'",
    ];
    let frequences = ["44100", "96000", "192000", "NULL"];
    let resolutions = ["16", "24", "NULL", "32"];
    let mut sql = Vec::new();
    for a in 1..=12 {
        sql.push(format!(
            "INSERT INTO artists (id, name) VALUES ({}, 'Artiste {a}{suffixe}')",
            base + a
        ));
    }
    let mut piste = 0;
    for i in 1..=200i64 {
        let annee = if i % 5 == 0 {
            "NULL".to_string()
        } else {
            (1990 + i % 40).to_string()
        };
        let pochette = if i % 7 == 0 {
            "NULL".to_string()
        } else if i % 11 == 0 {
            "''".to_string()
        } else {
            format!("'/c/{i}.jpg'")
        };
        let label = if i % 3 == 0 { "'ECM'" } else { "'Blue Note'" };
        sql.push(format!(
            "INSERT INTO albums (id, title, artist_id, year, cover_path, genre, label) \
             VALUES ({}, 'Album {i}{suffixe}', {}, {annee}, {pochette}, NULL, {label})",
            base + i,
            base + i % 12 + 1
        ));
        if i > 190 {
            continue;
        }
        for n in 1..=(1 + i % 9) {
            piste += 1;
            let dsd = i % 13 == 0;
            let format = if dsd {
                "'dsd'"
            } else if n == 2 && i % 4 == 0 {
                "'WAV'"
            } else {
                "'flac'"
            };
            let racine = if i % 2 == 0 { "a" } else { "b" };
            let ext = if dsd { "dsf" } else { "flac" };
            let annee_piste = if (i + n) % 6 == 0 { "2026" } else { "NULL" };
            sql.push(format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                 file_path, format, genre, sample_rate, bit_depth, year, file_mtime, duration_ms) \
                 VALUES ({}, 'Piste {n}{suffixe}', {}, {}, 1, {n}, '/m/{racine}/{i}{suffixe}/{n}.{ext}', \
                 {format}, {}, {}, {}, {annee_piste}, {}, {})",
                base + piste,
                base + i,
                base + i % 12 + 1,
                genres[((i + n) % 7) as usize],
                frequences[((i + n) % 4) as usize],
                resolutions[((i * n) % 4) as usize],
                maintenant - (i % 200) * 86_400,
                200_000 + n * 30_000,
            ));
        }
    }
    sql
}

/// Une base SQLite migrée (index de la passe de démarrage compris), remplie
/// et analysée.
fn sqlite() -> Arc<dyn DbBackend> {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    let mut lot = String::from("BEGIN;\n");
    for l in jeu_d_essai(0, "") {
        lot.push_str(&l);
        lot.push_str(";\n");
    }
    lot.push_str("COMMIT;\nANALYZE;\n");
    db.execute_batch(&lot).expect("remplissage");
    Arc::new(db)
}

fn entier(db: &dyn DbBackend, sql: &str, colonne: usize) -> i64 {
    db.query_many(sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .first()
        .and_then(|r| r.get(colonne))
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("pas de compte\n{sql}"))
}

/// Pour chaque règle : (anciens comptes, nouveaux comptes).
fn comparer(db: &Arc<dyn DbBackend>, restreindre: &str) -> Vec<(String, (i64, i64), (i64, i64))> {
    let resolveur = DbRefResolver::new(db);
    let ctx = RefCtx::root(&resolveur, Some(1));
    REGLES
        .iter()
        .map(|(mode, regles)| {
            let (w, _o, _l) = build_album_query(regles, mode, "title", "asc", None, &ctx);
            // Sur la base PostgreSQL partagée, le compte est borné au jeu
            // de l'épreuve — la même borne pour l'ancien et le nouveau SQL.
            let w = if restreindre.is_empty() {
                w
            } else if w.is_empty() {
                format!("WHERE {restreindre}")
            } else {
                format!(
                    "WHERE ({}) AND {restreindre}",
                    w.trim_start_matches("WHERE ")
                )
            };
            let avant = (
                entier(db.as_ref(), &sql_compte_de_collection("al.id", &w), 0),
                entier(db.as_ref(), &sql_compte_de_collection("t.id", &w), 0),
            );
            let un = sql_comptes_de_collection(&w);
            let apres = (entier(db.as_ref(), &un, 0), entier(db.as_ref(), &un, 1));
            (format!("{mode} {regles}"), avant, apres)
        })
        .collect()
}

fn verifier(resultats: &[(String, (i64, i64), (i64, i64))]) {
    for (regle, avant, apres) in resultats {
        assert_eq!(
            avant, apres,
            "les comptes en une passe doivent égaler les deux requêtes d'avant : {regle}"
        );
    }
    // Un jeu qui ne compterait rien ne prouverait rien.
    let non_nuls = resultats
        .iter()
        .filter(|(_, a, _)| a.0 > 0 && a.1 > 0)
        .count();
    assert!(
        non_nuls >= REGLES.len() - 2,
        "le jeu d'essai doit nourrir presque toutes les règles : {resultats:?}"
    );
    // Les albums sans piste comptent pour une colonne de piste « vide ».
    let vide = resultats
        .iter()
        .find(|(r, _, _)| r.contains(r#""field":"genre","operator":"is_empty""#))
        .expect("règle « genre vide »");
    assert!(
        vide.1.0 >= 10,
        "les 10 albums sans piste répondent à « genre vide » : {vide:?}"
    );
}

#[test]
fn comptes_en_une_passe_egalent_les_deux_requetes() {
    verifier(&comparer(&sqlite(), ""));
}

/// Le même jeu, les mêmes réponses, sur PostgreSQL (`TUNE_TEST_PG_URL`),
/// dans un schéma à l'épreuve. SAUT sans URL ; posée mais injoignable, le
/// test tombe.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_comptes_collections_en_une_passe() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let schema = "comptes_collections_une_passe";
    let racine = tune_core::db::postgres::PostgresDb::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let admin: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        racine.pool().clone(),
    ));
    tokio::task::block_in_place(|| {
        admin
            .execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
            .unwrap();
        admin
            .execute(&format!("CREATE SCHEMA {schema}"), &[])
            .unwrap();
    });
    let sep = if url.contains('?') { '&' } else { '?' };
    let pg = tune_core::db::postgres::PostgresDb::connect(&format!(
        "{url}{sep}options=-csearch_path%3D{schema}"
    ))
    .await
    .expect("connexion au schéma d'essai");
    let db: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    ));
    let resultats = tokio::task::block_in_place(|| {
        for ddl in [
            "CREATE TABLE artists (id BIGINT PRIMARY KEY, name TEXT)",
            "CREATE TABLE albums (id BIGINT PRIMARY KEY, title TEXT NOT NULL, artist_id BIGINT, \
             year INTEGER, original_year INTEGER, cover_path TEXT, genre TEXT, label TEXT, \
             track_count INTEGER DEFAULT 0, is_compilation SMALLINT DEFAULT 0, \
             release_date TEXT, original_date TEXT)",
            "CREATE TABLE tracks (id BIGINT PRIMARY KEY, title TEXT NOT NULL, album_id BIGINT, \
             artist_id BIGINT, disc_number INTEGER, track_number INTEGER, file_path TEXT, \
             format TEXT, genre TEXT, sample_rate INTEGER, bit_depth INTEGER, year INTEGER, \
             file_mtime DOUBLE PRECISION, duration_ms BIGINT, composer TEXT, source TEXT, \
             album_artist TEXT, bpm INTEGER, rating INTEGER)",
        ] {
            db.execute(ddl, &[])
                .unwrap_or_else(|e| panic!("{e}\n{ddl}"));
        }
        for l in jeu_d_essai(0, "") {
            db.execute(&l, &[]).unwrap_or_else(|e| panic!("{e}\n{l}"));
        }
        let r = comparer(&db, "");
        admin
            .execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
            .ok();
        r
    });
    verifier(&resultats);
}

/// Le plan d'une requête, une étape par ligne.
fn plan(db: &Arc<dyn DbBackend>, sql: &str) -> Vec<String> {
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
        .expect("plan")
        .iter()
        .map(|r| r.last().and_then(|v| v.as_string()).unwrap_or_default())
        .collect()
}

/// Les étapes du plan qui relisent la TABLE des pistes (`t` ou `t2`) au lieu
/// d'un index qui porte tout ce que la requête en lit.
fn lectures_de_table(etapes: &[String]) -> Vec<String> {
    etapes
        .iter()
        .filter(|e| {
            let e = e.trim();
            (e.starts_with("SCAN t ")
                || e.starts_with("SEARCH t ")
                || e.starts_with("SCAN t2")
                || e.starts_with("SEARCH t2")
                || e == "SCAN t")
                && !e.contains("COVERING INDEX")
        })
        .cloned()
        .collect()
}

/// Les règles mesurées lentes sur le .18 le 14/11 (journal `slow_query`) :
/// format, fréquence croisée au genre, répertoire, année, date d'ajout.
const REGLES_LENTES: &[&str] = &[
    r#"[{"field":"format","operator":"equals","value":"dsd"}]"#,
    r#"[{"field":"sample_rate","operator":"greater_than","value":"96000"},{"field":"genre","op":"=","value":"Jazz"}]"#,
    r#"[{"field":"folder","op":"starts_with","value":"/m/b/"}]"#,
    r#"[{"field":"year","op":"=","value":2026}]"#,
    r#"[{"field":"added_at","operator":"greater_than","value":"90d"}]"#,
];

fn tables_relues(db: &Arc<dyn DbBackend>) -> Vec<String> {
    let resolveur = DbRefResolver::new(db);
    let ctx = RefCtx::root(&resolveur, Some(1));
    let mut relues = Vec::new();
    for regles in REGLES_LENTES {
        let (w, o, l) = build_album_query(regles, "all", "title", "asc", Some(64), &ctx);
        for (quoi, sql) in [
            ("comptes", sql_comptes_de_collection(&w)),
            ("pochettes", sql_albums_de_collection(&w, &o, &l)),
        ] {
            for e in lectures_de_table(&plan(db, &sql)) {
                relues.push(format!("{quoi} {regles} : {e}"));
            }
        }
    }
    relues
}

#[test]
fn les_regles_audio_annee_ajout_et_repertoire_ne_lisent_que_des_index() {
    let db = sqlite();
    let relues = tables_relues(&db);
    assert!(
        relues.is_empty(),
        "recette du 13/11 (n° 73) : la jointure relit la table des pistes au lieu \
         des index (album, colonnes des règles) :\n{}",
        relues.join("\n")
    );
    // Contre-épreuve : sans les deux index, la même lecture relit la table —
    // l'épreuve sait donc voir ce qu'elle garde.
    db.execute("DROP INDEX idx_tracks_album_audio", &[])
        .expect("retrait");
    db.execute("DROP INDEX idx_tracks_album_path", &[])
        .expect("retrait");
    db.execute("ANALYZE", &[]).expect("analyse");
    assert!(
        !tables_relues(&db).is_empty(),
        "sans les index, le plan devrait relire la table des pistes"
    );
}

/// Et la LISTE des collections porte ces comptes : le handler, sur les
/// collections du semis et sur celles du tableau, rend les comptes que les
/// deux anciennes requêtes donnaient. Débrancher la seconde colonne (le compte
/// de pistes) ou la lire dans la mauvaise colonne fait tomber l'épreuve.
#[tokio::test]
async fn la_liste_porte_les_comptes_d_avant() {
    use tune_core::db::backend::ToSqlValue;
    let db = sqlite();
    for (i, (mode, regles)) in REGLES.iter().enumerate() {
        db.execute(
            "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order) \
             VALUES (?1, ?2, ?3, 'title', 'asc')",
            &[
                &format!("Épreuve {i:02}") as &dyn ToSqlValue,
                &regles.to_string(),
                &mode.to_string(),
            ],
        )
        .expect("collection");
    }
    let etat = crate::SmartHttpState::new(db.clone());
    let Ok(axum::Json(liste)) = super::list_collections(
        axum::extract::State(etat),
        tune_http_types::ActiveProfile(1),
    )
    .await
    else {
        panic!("la liste doit répondre");
    };
    let resolveur = DbRefResolver::new(&db);
    let ctx = RefCtx::root(&resolveur, Some(1));
    let lignes = liste.as_array().expect("tableau");
    let mut vues = 0;
    for c in lignes {
        let regles = c["rules"].to_string();
        let mode = c["match_mode"].as_str().unwrap_or("all");
        if c.get("track_count_partiel").is_some() || c["catalogue_service"] != Value::Null {
            continue;
        }
        let (w, _o, _l) = build_album_query(&regles, mode, "title", "asc", None, &ctx);
        let albums = entier(db.as_ref(), &sql_compte_de_collection("al.id", &w), 0);
        let pistes = entier(db.as_ref(), &sql_compte_de_collection("t.id", &w), 0);
        assert_eq!(c["album_count"].as_i64(), Some(albums), "{c}");
        assert_eq!(c["track_count"].as_i64(), Some(pistes), "{c}");
        vues += 1;
    }
    assert!(
        vues >= REGLES.len(),
        "{vues} collections comparées sur {}",
        lignes.len()
    );
}
