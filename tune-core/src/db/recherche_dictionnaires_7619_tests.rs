//! #7619 (L3) — témoins de la recherche de pistes réécrite : les trois `LIKE`
//! sans accents posés sur des DICTIONNAIRES (artistes, genres et compositeurs
//! distincts) au lieu de chaque piste, et la page lue avec son total en une
//! seule requête.
//!
//! Témoin de RÉSULTAT : l'ancien prédicat (`sql::search_avant_7619`, la
//! rédaction d'avant gardée pour les témoins) et le nouveau rendent les mêmes pistes,
//! dans le même ordre, et le même total — accents, casse, `NULL`, artiste
//! absent, album masqué, année compris.
//!
//! Témoin de PLAN : la recherche ne parcourt plus la table des pistes ; chaque
//! branche passe par son index, et la piste par sa clé.

use std::sync::Arc;

use super::backend::DbBackend;
use super::engine::SqliteDialect;
use super::sqlite::SqliteDb;
use super::track_repo::{TrackRepo, sql};

fn base() -> (Arc<SqliteDb>, TrackRepo) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES
           (1, 'The Beatles'), (2, 'Beyoncé'), (3, 'ÉBÈNE QUARTET'), (4, 'Ólafur Arnalds'),
           (5, 'Miles Davis'), (6, 'abeat collective');
         INSERT INTO albums (id, title, artist_id, year) VALUES
           (1, 'Abbey Road', 1, 1969), (2, 'Lemonade', 2, 2016), (3, 'Fiction', 3, 2010),
           (4, 'Re:member', 4, 2018), (5, 'Kind of Blue', 5, 1959), (6, 'Caché', 6, 1975),
           (7, 'Album masqué', 1, 1975);
         INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 7);
         INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path) VALUES
           (1, 'Come Together', 1, 1, 'Rock', 'Lennon', '/m/1.flac'),
           (2, 'Something', 1, 1, 'rock', NULL, '/m/2.flac'),
           (3, 'Formation', 2, 2, 'R&B', 'Beyoncé Knowles', '/m/3.flac'),
           (4, 'Libertango', 3, 3, 'Jazz', 'Piazzolla', '/m/4.flac'),
           (5, 'Saman', 4, 4, 'Électronique', 'Ólafur', '/m/5.flac'),
           (6, 'So What', 5, 5, 'JAZZ', 'Davis', '/m/6.flac'),
           (7, 'Beat It', 6, 6, NULL, 'Dvořák', '/m/7.flac'),
           (8, 'Orpheline', NULL, NULL, 'electro', NULL, '/m/8.flac'),
           (9, 'Cachée', 7, 1, 'Rock', 'Lennon', '/m/9.flac'),
           (10, 'Sans rien', NULL, NULL, NULL, NULL, '/m/10.flac');",
    )
    .unwrap();
    let db = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    (db, repo)
}

const REQUETES: [&str; 18] = [
    // Au milieu d'un mot, accents dans la donnée : seul le `LIKE` les trouve.
    "orak",
    "ectroniq",
    "beat",
    "BEAT",
    "beyonce",
    "BEYONCÉ",
    "ebene",
    "electro",
    "élect",
    "jazz",
    "dvorak",
    "1975",
    "1969",
    "e",
    "olafur",
    "\"come together\"",
    "zzqx",
    "rock",
];

/// L'ancien chemin et le nouveau rendent les mêmes pistes, dans le même ordre,
/// et le même total ; la page lue avec son total ([`TrackRepo::
/// search_page_et_total`]) vaut la page et le compte lus séparément.
#[test]
fn memes_pistes_meme_ordre_meme_total_que_l_ancien_chemin() {
    let (db, repo) = base();
    let page_ancienne = sql::search_avant_7619(&SqliteDialect);
    let compte_ancien = sql::search_count_avant_7619(&SqliteDialect);
    let mut non_vides = 0;
    for q in REQUETES {
        let fts = crate::db::engine::format_fts_query_piste_ou_chemin(
            crate::db::engine::Engine::Sqlite,
            q,
        );
        let fts_id =
            crate::db::engine::format_fts_query_piste(crate::db::engine::Engine::Sqlite, q);
        let like = crate::db::engine::motif_like(q);
        let trimmed = q.trim().to_string();
        let ids_anciens: Vec<i64> = db
            .query_many(
                &page_ancienne,
                &[
                    &fts, &like, &like, &like, &trimmed, &fts_id, &like, &like, &like, &trimmed,
                    &1000_i64, &0_i64,
                ],
            )
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        let total_ancien = db
            .query_one(
                &compte_ancien,
                &[&fts, &like, &like, &like, &trimmed, &5000_i64],
            )
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap();

        let (pistes, total) = repo.search_page_et_total(q, 1000, 0, 5000).unwrap();
        let ids: Vec<i64> = pistes.iter().map(|t| t.id.unwrap()).collect();
        assert_eq!(ids, ids_anciens, "« {q} » : pistes ou ordre différents");
        assert_eq!(total, total_ancien, "« {q} » : total différent");
        assert_eq!(
            repo.search_page(q, 1000, 0)
                .unwrap()
                .iter()
                .map(|t| t.id.unwrap())
                .collect::<Vec<_>>(),
            ids_anciens,
            "« {q} » : search_page diffère"
        );
        assert_eq!(repo.search_count(q, 5000).unwrap(), total_ancien);
        // Pages de 2, plafond de 3 : le total est borné comme `search_count`,
        // et une page au-delà de la fin relit le compte.
        for offset in [0_i64, 2, 50] {
            let (p, t) = repo.search_page_et_total(q, 2, offset, 3).unwrap();
            let attendu: Vec<i64> = ids_anciens
                .iter()
                .skip(offset as usize)
                .take(2)
                .copied()
                .collect();
            assert_eq!(p.iter().map(|t| t.id.unwrap()).collect::<Vec<_>>(), attendu);
            assert_eq!(
                t,
                total_ancien.min(3),
                "« {q} » offset {offset} : total borné"
            );
        }
        if !ids_anciens.is_empty() {
            non_vides += 1;
        }
    }
    // Le témoin n'est pas vide : la plupart des requêtes trouvent quelque chose.
    assert!(non_vides >= 12, "{non_vides} requêtes non vides seulement");
    // Les cas qui font la différence : préfixe, accents et casse, NULL.
    let ids = |q: &str| -> Vec<i64> {
        repo.search_page(q, 100, 0)
            .unwrap()
            .iter()
            .map(|t| t.id.unwrap())
            .collect()
    };
    assert!(
        ids("beat").contains(&1),
        "« beat » doit trouver les Beatles"
    );
    assert!(ids("beyonce").contains(&3), "accents de l'artiste");
    assert!(ids("ELECTRO").contains(&5), "accents et casse du genre");
    assert!(ids("dvorak").contains(&7), "accents du compositeur");
    assert!(
        ids("orak").contains(&7),
        "milieu de mot, accents du compositeur"
    );
    assert!(
        ids("ectroniq").contains(&5),
        "milieu de mot, accents du genre"
    );
    assert!(
        ids("eat").contains(&7),
        "milieu de mot de l'artiste (abeat)"
    );
    assert!(ids("1975").contains(&7), "année de l'album");
    assert!(!ids("1975").contains(&9), "album masqué exclu");
}

/// Le plan : plus aucun parcours de la table des pistes ; chaque branche passe
/// par son index (plein texte, artiste, genre, compositeur, album).
#[test]
fn la_recherche_ne_parcourt_plus_la_table_des_pistes() {
    let (db, _repo) = base();
    // Une bibliothèque à l'échelle : 300 artistes et 300 albums de 10 pistes,
    // 40 genres, 300 compositeurs.
    let mut sql = String::from("BEGIN;");
    for a in 100..400 {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\
             INSERT INTO albums (id, title, artist_id, year) VALUES ({a}, 'Album {a}', {a}, {});",
            1950 + a % 70
        ));
    }
    for i in 100..3100 {
        let al = 100 + (i - 100) / 10;
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path) \
             VALUES ({i}, 'Titre {i}', {al}, {al}, 'Genre {}', 'Compositeur {}', '/m/{i}.flac');",
            i % 40,
            i % 300
        ));
    }
    sql.push_str("COMMIT; ANALYZE;");
    db.execute_batch(&sql).unwrap();
    let plan: Vec<String> = db
        .query_many(
            &format!("EXPLAIN QUERY PLAN {}", sql::search(&SqliteDialect)),
            &[
                &"beat*", &"%beat%", &"%beat%", &"%beat%", &"beat", &"beat*", &"%beat%", &"%beat%",
                &"%beat%", &"beat", &20_i64, &0_i64,
            ],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(3).and_then(|v| v.as_string()).unwrap_or_default())
        .collect();
    let plan = plan.join("\n");
    assert!(
        plan.contains("SEARCH t USING INTEGER PRIMARY KEY"),
        "la piste n'est pas lue par sa clé :\n{plan}"
    );
    assert!(
        !plan.lines().any(|l| l.trim() == "SCAN t"),
        "la recherche parcourt toute la table des pistes :\n{plan}"
    );
    assert!(plan.contains("idx_tracks_genre"), "{plan}");
    assert!(plan.contains("idx_tracks_composer"), "{plan}");
}

/// BANC sur une COPIE de la base d'un serveur (`TUNE_BANC_DB`) — `#[ignore]`,
/// à la main, en `--release`. La copie est migrée (la passe de démarrage pose
/// les deux index) : ne jamais le pointer sur une base de production.
///
/// ```text
/// TUNE_BANC_DB=/tmp/copie.db cargo test --release -p tune-core --lib \
///   banc_recherche_7619_sur_copie -- --ignored --nocapture
/// ```
///
/// Pour chaque requête : ancien chemin (page + compte, deux passes) et nouveau
/// (une requête), médiane de 5 après une chauffe, et MÊMES pistes, même ordre,
/// même total — sur les données réelles.
#[test]
#[ignore = "banc manuel sur une copie de base, --release"]
fn banc_recherche_7619_sur_copie() {
    use std::time::Instant;
    let Ok(chemin) = std::env::var("TUNE_BANC_DB") else {
        eprintln!("TUNE_BANC_DB absent : banc sauté");
        return;
    };
    let db = SqliteDb::open(&chemin).expect("copie de base");
    crate::db::migrations::run_migrations(&db).unwrap();
    let db = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    let page_ancienne = sql::search_avant_7619(&SqliteDialect);
    let compte_ancien = sql::search_count_avant_7619(&SqliteDialect);
    let requetes = std::env::var("TUNE_BANC_REQUETES")
        .unwrap_or_else(|_| "beat,bach,miles davis,jazz,1975,beyonce,mozart,é,zzqx".into());
    let mediane = |f: &mut dyn FnMut()| {
        f();
        let mut d: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        d[2]
    };
    for q in requetes.split(',') {
        let fts = crate::db::engine::format_fts_query_piste_ou_chemin(
            crate::db::engine::Engine::Sqlite,
            q,
        );
        let fts_id =
            crate::db::engine::format_fts_query_piste(crate::db::engine::Engine::Sqlite, q);
        let like = crate::db::engine::motif_like(q);
        let trimmed = q.trim().to_string();
        let mut ancien_res = (Vec::new(), 0_i64);
        let t_ancien = mediane(&mut || {
            let ids: Vec<i64> = db
                .query_many(
                    &page_ancienne,
                    &[
                        &fts, &like, &like, &like, &trimmed, &fts_id, &like, &like, &like,
                        &trimmed, &50_i64, &0_i64,
                    ],
                )
                .unwrap()
                .iter()
                .map(|r| r[0].as_i64().unwrap())
                .collect();
            let n = db
                .query_one(
                    &compte_ancien,
                    &[&fts, &like, &like, &like, &trimmed, &5000_i64],
                )
                .unwrap()
                .unwrap()[0]
                .as_i64()
                .unwrap();
            ancien_res = (ids, n);
        });
        let mut neuf_res = (Vec::new(), 0_i64);
        let t_neuf = mediane(&mut || {
            let (p, n) = repo.search_page_et_total(q, 50, 0, 5000).unwrap();
            neuf_res = (p.iter().map(|t| t.id.unwrap()).collect(), n);
        });
        eprintln!(
            "BANC recherche {q:<12} ancien={t_ancien:>7.1} ms  nouveau={t_neuf:>6.1} ms  \
             total={}  identiques={}",
            neuf_res.1,
            ancien_res == neuf_res
        );
        assert_eq!(ancien_res, neuf_res, "« {q} » : résultats différents");
    }
}

/// Le texte libre d'Oxygen (`/library/tracks?q=`) : label et compositeur sur
/// leurs dictionnaires — mêmes pistes que l'ancien prédicat, où le `LIKE`
/// portait sur chaque piste.
#[test]
fn texte_libre_memes_pistes_que_l_ancien_predicat() {
    use crate::db::facet_filter::{Placeholders, condition_texte_libre};
    let (db, _repo) = base();
    db.execute_batch(
        "UPDATE tracks SET label = 'Parlophone' WHERE id IN (1, 2);
         UPDATE tracks SET label = 'ÉCM Records' WHERE id = 4;
         UPDATE tracks SET label = 'columbia' WHERE id = 6;",
    )
    .unwrap();
    let esc = crate::db::track_repo::like_escape_clause();
    let mut non_vides = 0;
    for q in [
        "parlo",
        "ecm",
        "ÉCM",
        "COLUMB",
        "dvorak",
        "lennon",
        "beat",
        "e",
        "50%",
        "zzqx",
        "come together",
    ] {
        let mut ph = Placeholders::resuming_at(crate::db::engine::Engine::Sqlite, 1);
        let (cond, valeurs) = condition_texte_libre(&mut ph, q);
        let mut vieux = cond.clone();
        for col in ["label", "composer"] {
            let neuf = format!(
                "t.{col} IN (WITH dico AS MATERIALIZED \
                 (SELECT DISTINCT {col} AS v FROM tracks WHERE {col} IS NOT NULL) \
                 SELECT dico.v FROM dico WHERE LOWER(unaccent(dico.v)) LIKE LOWER(unaccent(?)){esc})"
            );
            assert!(vieux.contains(&neuf), "« {q} » : {col} absent : {vieux}");
            vieux = vieux.replace(
                &neuf,
                &format!("LOWER(unaccent(t.{col})) LIKE LOWER(unaccent(?)){esc}"),
            );
        }
        let refs: Vec<&dyn super::backend::ToSqlValue> = valeurs
            .iter()
            .map(|v| v as &dyn super::backend::ToSqlValue)
            .collect();
        let ids = |c: &str| -> Vec<i64> {
            db.query_many(
                &format!("SELECT t.id FROM tracks t WHERE {c} ORDER BY t.id"),
                &refs,
            )
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect()
        };
        let (a, n) = (ids(&vieux), ids(&cond));
        assert_eq!(n, a, "« {q} » : texte libre différent");
        if !a.is_empty() {
            non_vides += 1;
        }
    }
    assert!(non_vides >= 7, "{non_vides}");
}
