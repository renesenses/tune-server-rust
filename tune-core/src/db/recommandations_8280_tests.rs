//! Discussion #8280 — « À découvrir » varie ses raisons : les requêtes des
//! trois nouvelles sources (artiste possédé, même époque, peu écouté),
//! exécutées sur une vraie base et mesurées sur 10 000 albums.
//!
//! SQLite toujours ; PostgreSQL quand `TUNE_TEST_PG_URL` est posée (job
//! « Test (PostgreSQL) »), sur le même banc.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::Engine;
use super::home_queries::{
    albums_d_artistes_possedes_non_ecoutes, albums_de_la_decennie_non_ecoutes,
    albums_du_genre_non_ecoutes, albums_peu_ecoutes, decennie_la_plus_ecoutee, top_genres_ecoutes,
};
use super::sqlite::SqliteDb;

const ARTISTES: i64 = 500;
const DIVERS: i64 = ARTISTES + 1;
const INCONNU: i64 = ARTISTES + 2;
/// La date avant laquelle la dernière écoute d'un « peu écouté » tombe.
const AVANT: &str = "2026-10-15";

/// Le banc : `albums` albums de 500 artistes (20 chacun pour 10 000),
/// années 1950 à 2009, cinq genres ; 30 albums « Various Artists » et 30
/// « Unknown Artist ». Le premier quart est bien écouté (12 écoutes chacun
/// pour 10 000 albums et 30 000 écoutes), les 200 suivants une fois, dont la
/// moitié par le repli titre + artiste (`album_id` NULL), et le suivant une
/// fois mais récemment.
fn banc(albums: i64, ecoutes: i64) -> Vec<String> {
    let mut sql = Vec::new();
    let mut artistes: Vec<String> = (1..=ARTISTES)
        .map(|i| format!("({i}, 'Artiste {i}')"))
        .collect();
    artistes.push(format!("({DIVERS}, 'Various Artists')"));
    artistes.push(format!("({INCONNU}, 'Unknown Artist')"));
    sql.push(format!(
        "INSERT INTO artists (id, name) VALUES {}",
        artistes.join(", ")
    ));
    let genres = ["Rock", "Jazz", "Pop", "Folk", "Soul"];
    let mut lignes: Vec<String> = (1..=albums)
        .map(|i| {
            format!(
                "({i}, 'Album {i}', {}, {}, '{}')",
                i % ARTISTES + 1,
                1950 + i % 60,
                genres[(i % 5) as usize]
            )
        })
        .collect();
    for k in 1..=30 {
        lignes.push(format!(
            "({}, 'Divers {k}', {DIVERS}, 1975, 'Pop')",
            albums + k
        ));
        lignes.push(format!(
            "({}, 'Inconnu {k}', {INCONNU}, 1975, 'Pop')",
            albums + 30 + k
        ));
    }
    for paquet in lignes.chunks(1_000) {
        sql.push(format!(
            "INSERT INTO albums (id, title, artist_id, year, genre) VALUES {}",
            paquet.join(", ")
        ));
    }

    let quart = (albums / 4).max(1);
    let mut historique: Vec<String> = (0..ecoutes)
        .map(|k| {
            let a = k % quart + 1;
            format!(
                "('Piste {k}', 'Artiste {}', 'Album {a}', {a}, '2026-10-01T10:00:00Z')",
                a % ARTISTES + 1
            )
        })
        .collect();
    for a in quart + 1..=quart + 200 {
        let cle = if a % 2 == 0 {
            a.to_string()
        } else {
            "NULL".into()
        };
        historique.push(format!(
            "('Piste', 'Artiste {}', 'Album {a}', {cle}, '2026-10-02T10:00:00Z')",
            a % ARTISTES + 1
        ));
    }
    // Écouté une fois, mais APRÈS `AVANT` : en cours, pas « peu écouté ».
    let recent = quart + 201;
    historique.push(format!(
        "('Piste', 'Artiste {}', 'Album {recent}', {recent}, '2026-11-01T10:00:00Z')",
        recent % ARTISTES + 1
    ));
    for paquet in historique.chunks(1_000) {
        sql.push(format!(
            "INSERT INTO listen_history (title, artist_name, album_title, album_id, listened_at) VALUES {}",
            paquet.join(", ")
        ));
    }
    sql
}

fn banc_sqlite(albums: i64, ecoutes: i64) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let mut sql = String::from("BEGIN;\n");
    for instruction in banc(albums, ecoutes) {
        sql.push_str(&instruction);
        sql.push_str(";\n");
    }
    sql.push_str("COMMIT;");
    db.execute_batch(&sql).unwrap();
    Arc::new(db)
}

fn lignes(db: &dyn DbBackend, sql: &str, params: &[&dyn ToSqlValue]) -> Vec<Vec<SqlValue>> {
    db.query_many(sql, params)
        .unwrap_or_else(|e| panic!("la requête doit répondre : {e}\n{sql}"))
}

fn entier(cols: &[SqlValue], i: usize) -> i64 {
    cols.get(i).and_then(SqlValue::as_i64).unwrap_or(-1)
}

/// Les écoutes de chaque album selon le banc (clé ET repli), et l'artiste
/// de chaque album : la RÉFÉRENCE, calculée sans SQL.
struct Reference {
    ecoutes: BTreeMap<i64, i64>,
    albums_par_artiste: BTreeMap<i64, i64>,
}

fn reference(albums: i64, ecoutes: i64) -> Reference {
    let quart = (albums / 4).max(1);
    let mut e = BTreeMap::new();
    for k in 0..ecoutes {
        *e.entry(k % quart + 1).or_insert(0) += 1;
    }
    for a in quart + 1..=quart + 201 {
        *e.entry(a).or_insert(0) += 1;
    }
    let mut par_artiste = BTreeMap::new();
    for i in 1..=albums {
        *par_artiste.entry(i % ARTISTES + 1).or_insert(0) += 1;
    }
    par_artiste.insert(DIVERS, 30);
    par_artiste.insert(INCONNU, 30);
    Reference {
        ecoutes: e,
        albums_par_artiste: par_artiste,
    }
}

/// Les preuves, communes aux deux moteurs.
fn les_sources_disent_vrai(db: &dyn DbBackend, engine: Engine, albums: i64, ecoutes: i64) {
    let r = reference(albums, ecoutes);
    let tout: i64 = 1_000_000;

    // Artiste possédé : jamais écouté, artiste à ≥ 2 albums, jamais l'inconnu.
    let inconnu = "Unknown Artist";
    let rangs = lignes(
        db,
        &albums_d_artistes_possedes_non_ecoutes(engine),
        &[&inconnu as &dyn ToSqlValue, &tout],
    );
    assert!(!rangs.is_empty(), "aucun album d'artiste possédé");
    for cols in &rangs {
        let id = entier(cols, 0);
        assert!(!r.ecoutes.contains_key(&id), "album {id} déjà écouté");
        let nom = cols
            .get(2)
            .and_then(SqlValue::as_string)
            .unwrap_or_default();
        assert_ne!(nom, inconnu);
        let artiste = if nom == "Various Artists" {
            DIVERS
        } else {
            nom.trim_start_matches("Artiste ").parse().unwrap()
        };
        assert_eq!(
            entier(cols, 6),
            r.albums_par_artiste[&artiste],
            "album {id}"
        );
    }
    let attendus = (1..=albums + 30)
        .filter(|id| !r.ecoutes.contains_key(id))
        .count();
    assert_eq!(
        rangs.len(),
        attendus,
        "tous les jamais écoutés hors inconnu"
    );

    // Décennie la plus écoutée : la référence la calcule à part.
    let mut par_decennie: BTreeMap<i64, i64> = BTreeMap::new();
    for (id, n) in &r.ecoutes {
        *par_decennie.entry((1950 + id % 60) / 10 * 10).or_insert(0) += n;
    }
    let max = par_decennie.values().copied().max().unwrap();
    let attendue = *par_decennie.iter().find(|(_, n)| **n == max).unwrap().0;
    let d = lignes(db, &decennie_la_plus_ecoutee(), &[]);
    assert_eq!(d.len(), 1);
    assert_eq!(entier(&d[0], 0), attendue, "décennie");
    assert_eq!(entier(&d[0], 1), max, "écoutes de la décennie");

    let fin = attendue + 10;
    let rangs = lignes(
        db,
        &albums_de_la_decennie_non_ecoutes(engine),
        &[&attendue as &dyn ToSqlValue, &fin, &tout],
    );
    assert!(!rangs.is_empty(), "aucun album de la décennie");
    for cols in &rangs {
        let (id, annee) = (entier(cols, 0), entier(cols, 3));
        assert!((attendue..fin).contains(&annee), "album {id} : {annee}");
        assert!(!r.ecoutes.contains_key(&id), "album {id} déjà écouté");
    }

    // Peu écouté : 1 à 3 écoutes, le compte exact, repli compris.
    let plafond: i64 = 3;
    let rangs = lignes(
        db,
        &albums_peu_ecoutes(engine),
        &[&plafond as &dyn ToSqlValue, &AVANT, &tout],
    );
    let ids: BTreeSet<i64> = rangs.iter().map(|c| entier(c, 0)).collect();
    let recent = (albums / 4).max(1) + 201;
    let attendus: BTreeSet<i64> = r
        .ecoutes
        .iter()
        .filter(|(id, n)| (1..=plafond).contains(*n) && **id != recent)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(ids, attendus, "les albums à peine entamés");
    for cols in &rangs {
        assert_eq!(entier(cols, 6), r.ecoutes[&entier(cols, 0)]);
    }

    // Les bornes tiennent.
    let deux: i64 = 2;
    assert_eq!(
        lignes(
            db,
            &albums_peu_ecoutes(engine),
            &[&plafond as &dyn ToSqlValue, &AVANT, &deux]
        )
        .len(),
        2
    );
}

/// Les cinq requêtes de « À découvrir », jouées comme la route les joue.
fn duree_de_la_rangee(db: &dyn DbBackend, engine: Engine, limite: i64) -> Duration {
    let debut = Instant::now();
    let genres: Vec<String> = lignes(db, &top_genres_ecoutes(), &[])
        .into_iter()
        .filter_map(|c| c.first().and_then(SqlValue::as_string))
        .collect();
    let mut p: Vec<&dyn ToSqlValue> = genres.iter().map(|g| g as &dyn ToSqlValue).collect();
    p.push(&limite);
    lignes(db, &albums_du_genre_non_ecoutes(engine, genres.len()), &p);
    let inconnu = "Unknown Artist";
    lignes(
        db,
        &albums_d_artistes_possedes_non_ecoutes(engine),
        &[&inconnu as &dyn ToSqlValue, &limite],
    );
    let decennie = entier(&lignes(db, &decennie_la_plus_ecoutee(), &[])[0], 0);
    let fin = decennie + 10;
    lignes(
        db,
        &albums_de_la_decennie_non_ecoutes(engine),
        &[&decennie as &dyn ToSqlValue, &fin, &limite],
    );
    let plafond: i64 = 3;
    lignes(
        db,
        &albums_peu_ecoutes(engine),
        &[&plafond as &dyn ToSqlValue, &AVANT, &limite],
    );
    debut.elapsed()
}

#[test]
fn les_sources_disent_vrai_sqlite() {
    let db = banc_sqlite(2_000, 6_000);
    les_sources_disent_vrai(db.as_ref(), Engine::Sqlite, 2_000, 6_000);
}

/// Sans historique ni année, les sources se taisent — c'est ce qui laisse
/// le hasard reprendre.
#[test]
fn sans_donnees_les_sources_se_taisent() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Seul');\n\
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Unique', 1);",
    )
    .unwrap();
    let tout: i64 = 100;
    let inconnu = "Unknown Artist";
    let plafond: i64 = 3;
    assert!(
        lignes(
            &db,
            &albums_d_artistes_possedes_non_ecoutes(Engine::Sqlite),
            &[&inconnu as &dyn ToSqlValue, &tout]
        )
        .is_empty()
    );
    assert!(lignes(&db, &decennie_la_plus_ecoutee(), &[]).is_empty());
    assert!(
        lignes(
            &db,
            &albums_peu_ecoutes(Engine::Sqlite),
            &[&plafond as &dyn ToSqlValue, &AVANT, &tout]
        )
        .is_empty()
    );
}

/// La mesure demandée par #8280 : 10 000 albums, 30 000 écoutes. La rangée
/// entière (cinq requêtes) doit tenir largement dans la borne du widget
/// (3 s) — même en construction de débogage.
#[test]
fn mesure_10_000_albums_sqlite() {
    let db = banc_sqlite(10_000, 30_000);
    les_sources_disent_vrai(db.as_ref(), Engine::Sqlite, 10_000, 30_000);
    let duree = duree_de_la_rangee(db.as_ref(), Engine::Sqlite, 40);
    eprintln!("MESURE #8280 — SQLite, 10 060 albums, 30 200 écoutes : rangée en {duree:?}");
    assert!(duree < Duration::from_secs(3), "rangée en {duree:?}");
}

// ─── PostgreSQL ────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use crate::db::backend::PostgresBackend;

    async fn pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
        Some(sqlx::PgPool::connect(&url).await.unwrap_or_else(|e| {
            panic!("TUNE_TEST_PG_URL est posée ({url}) mais la connexion échoue : {e}")
        }))
    }

    fn charger(db: &Arc<dyn DbBackend>, albums: i64, ecoutes: i64) {
        for sql in crate::db::postgres::ENSURE_TABLES
            .iter()
            .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
        {
            let _ = db.execute(sql, &[]);
        }
        vider(db);
        for instruction in banc(albums, ecoutes) {
            db.execute(&instruction, &[])
                .unwrap_or_else(|e| panic!("banc : {e}\n{instruction}"));
        }
        db.execute("ANALYZE listen_history", &[]).unwrap();
        db.execute("ANALYZE albums", &[]).unwrap();
    }

    fn vider(db: &Arc<dyn DbBackend>) {
        for table in ["listen_history", "tracks", "albums", "artists"] {
            db.execute(
                &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
                &[],
            )
            .unwrap_or_else(|e| panic!("TRUNCATE {table} : {e}"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_8280_les_sources_disent_vrai_et_la_mesure() {
        let Some(pool) = pool().await else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));
        charger(&db, 10_000, 30_000);
        les_sources_disent_vrai(db.as_ref(), Engine::Postgres, 10_000, 30_000);
        let duree = duree_de_la_rangee(db.as_ref(), Engine::Postgres, 40);
        eprintln!("MESURE #8280 — PostgreSQL, 10 060 albums, 30 200 écoutes : rangée en {duree:?}");
        vider(&db);
        assert!(duree < Duration::from_secs(3), "rangée en {duree:?}");
    }
}
