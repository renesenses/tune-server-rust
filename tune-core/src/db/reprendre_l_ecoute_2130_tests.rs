//! Fil 2130 — « Reprendre l'écoute » tombait en « (delai) » sur PostgreSQL :
//! le second rang (`continue_listening_albums_deduits`) prenait 25,7 s pour
//! 8 571 albums, quand le widget abandonne à 8 s.
//!
//! La jointure `HISTORIQUE_VERS_ALBUM` (un `OR` entre la clé et un repli par
//! titre et artiste) est réécrite en deux branches `UNION ALL`
//! (`historique_rattache_a_son_album`), et `listen_history(album_id)` reçoit
//! un index (migration 115 / PG 079).
//!
//! Les preuves :
//!
//! * les DEUX requêtes du widget rendent exactement les lignes de l'ANCIENNE
//!   écriture, recopiée ici au caractère près, sur un banc qui couvre chaque
//!   cas de la règle (clé, repli par artiste, titre seul non ambigu, titre
//!   ambigu, artiste faux, album disparu, filtre de zone) ;
//! * la migration 115 monte sur une base existante, et la passe finale pose
//!   l'index même quand la version est déjà enregistrée ;
//! * sur PostgreSQL (`TUNE_TEST_PG_URL`) : mêmes lignes, la 079 pose l'index
//!   et se saute sans erreur quand la colonne n'existe pas encore, et le plan
//!   et la durée avant / après sont imprimés.

use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::Engine;
use super::home_queries::{
    continue_listening_albums_deduits, continue_listening_albums_du_contexte,
};
use super::sqlite::SqliteDb;

/// L'ANCIENNE `HISTORIQUE_VERS_ALBUM` (avant le fil 2130), recopiée au
/// caractère près : c'est la RÉFÉRENCE de ce que le widget rendait.
const ANCIENNE_JOINTURE: &str = "(lh.album_id = a.id \
     OR (lh.album_id IS NULL AND lh.album_title = a.title \
         AND ((COALESCE(lh.artist_name, '') <> '' \
               AND lh.artist_name \
                   = (SELECT ar_hist.name FROM artists ar_hist \
                      WHERE ar_hist.id = a.artist_id)) \
              OR (COALESCE(lh.artist_name, '') = '' \
                  AND NOT EXISTS (SELECT 1 FROM albums a_hom \
                                  WHERE a_hom.title = a.title \
                                    AND a_hom.id <> a.id)))))";

const COLONNES_ALBUM: &str = "a.id, a.title, ar.name, a.year, a.cover_path, a.genre";

/// L'ancien `continue_listening_albums_deduits`, au caractère près.
fn ancienne_deduits(engine: Engine, zone_filter: &str) -> String {
    let p1 = match engine {
        Engine::Sqlite => "?",
        Engine::Postgres => "$1",
    };
    format!(
        "SELECT {COLONNES_ALBUM}, \
               COUNT(DISTINCT lh.title) as listened_tracks, a.track_count, \
               MAX(lh.listened_at) as dernier \
        FROM listen_history lh \
        JOIN albums a ON {ANCIENNE_JOINTURE} \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE a.track_count IS NOT NULL AND a.track_count > 0 \
        {zone_filter}\
        GROUP BY {COLONNES_ALBUM}, a.track_count \
        HAVING COUNT(DISTINCT lh.title) < a.track_count \
           AND SUM(CASE WHEN lh.context_type IS NULL THEN 1 ELSE 0 END) > 0 \
        ORDER BY MAX(lh.listened_at) DESC \
        LIMIT {p1}"
    )
}

/// L'ancien `continue_listening_albums_du_contexte`, au caractère près.
fn ancienne_du_contexte(ids: &[i64]) -> String {
    let liste = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {COLONNES_ALBUM}, \
                COUNT(DISTINCT lh.title) as listened_tracks, a.track_count \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         LEFT JOIN listen_history lh ON {ANCIENNE_JOINTURE} \
         WHERE a.id IN ({liste}) \
         GROUP BY {COLONNES_ALBUM}, a.track_count"
    )
}

/// Premier identifiant des albums « uniques » du banc.
const PREMIER_UNIQUE: i64 = 10_000;
/// Premier identifiant des paires d'homonymes du banc.
const PREMIER_HOMONYME: i64 = 50_000;

/// Le banc, en instructions SQL valides sur les deux moteurs.
///
/// * `uniques` albums `Unique i`, artiste `i % 50 + 1`, 8 pistes — un sur
///   treize sans nombre de pistes, un sur dix-sept à zéro (le `WHERE` les
///   écarte) ;
/// * `uniques / 5` paires `Live j` chez « Homonyme A » et « Homonyme B »
///   (même titre, deux disques), 5 pistes ;
/// * `ecoutes` lignes d'historique, dans les huit situations de la règle :
///   0-1 clé valide sans contexte, 2 clé valide AVEC contexte, 3 repli par
///   titre et artiste, 4 repli sur un homonyme par son artiste, 5 titre seul
///   (artiste vide) — non ambigu sur un unique, ambigu sur un `Live`,
///   6 clé vers un album disparu, 7 artiste faux ;
/// * trois zones, réparties sur les écoutes.
fn banc(uniques: i64, ecoutes: i64) -> Vec<String> {
    let mut sql = Vec::new();
    sql.push(
        "INSERT INTO zones (id, name, output_type) VALUES \
         (1, 'Salon', 'local'), (2, 'Bureau', 'local'), (3, 'Cuisine', 'local')"
            .to_string(),
    );
    let mut artistes: Vec<String> = (1..=50).map(|i| format!("({i}, 'Artiste {i}')")).collect();
    artistes.push("(901, 'Homonyme A')".into());
    artistes.push("(902, 'Homonyme B')".into());
    artistes.push("(903, 'Homonyme C')".into());
    sql.push(format!(
        "INSERT INTO artists (id, name) VALUES {}",
        artistes.join(", ")
    ));

    let mut albums = Vec::new();
    for i in 0..uniques {
        let pistes = if i % 13 == 0 {
            "NULL".to_string()
        } else if i % 17 == 0 {
            "0".to_string()
        } else {
            "8".to_string()
        };
        albums.push(format!(
            "({}, 'Unique {i}', {}, {pistes})",
            PREMIER_UNIQUE + i,
            i % 50 + 1
        ));
    }
    let paires = uniques / 5;
    for j in 0..paires {
        albums.push(format!(
            "({}, 'Live {j}', 901, 5)",
            PREMIER_HOMONYME + 2 * j
        ));
        albums.push(format!(
            "({}, 'Live {j}', 902, 5)",
            PREMIER_HOMONYME + 2 * j + 1
        ));
    }
    // Une variante de CASSE d'un titre partagé : homonyme ou non selon la
    // collation, et la même des deux côtés de la comparaison.
    albums.push("(90000, 'live 1', 903, 5)".to_string());
    for paquet in albums.chunks(500) {
        sql.push(format!(
            "INSERT INTO albums (id, title, artist_id, track_count) VALUES {}",
            paquet.join(", ")
        ));
    }

    let mut lignes = Vec::new();
    for k in 0..ecoutes {
        let i = (k * 7) % uniques;
        let j = (k * 3) % paires.max(1);
        let unique = PREMIER_UNIQUE + i;
        let artiste = format!("'Artiste {}'", i % 50 + 1);
        let titre_unique = format!("'Unique {i}'");
        let (album_id, album_title, artist_name, contexte) = match k % 8 {
            0 | 1 => (unique.to_string(), titre_unique, artiste, "NULL"),
            2 => (unique.to_string(), titre_unique, artiste, "'album'"),
            3 => ("NULL".into(), titre_unique, artiste, "NULL"),
            4 => (
                "NULL".into(),
                format!("'Live {j}'"),
                "'Homonyme A'".into(),
                "NULL",
            ),
            5 if k % 16 == 5 => ("NULL".into(), titre_unique, "''".into(), "NULL"),
            5 => ("NULL".into(), format!("'Live {j}'"), "NULL".into(), "NULL"),
            6 => ("999999".into(), titre_unique, artiste, "NULL"),
            _ => ("NULL".into(), titre_unique, "'Inconnu'".into(), "NULL"),
        };
        lignes.push(format!(
            "('Piste {}', {artist_name}, {album_title}, {album_id}, {contexte}, {}, \
              '2026-09-{:02}T{:02}:{:02}:00Z')",
            k % 6,
            k % 3 + 1,
            k % 28 + 1,
            k % 24,
            k % 60
        ));
    }
    for paquet in lignes.chunks(500) {
        sql.push(format!(
            "INSERT INTO listen_history \
             (title, artist_name, album_title, album_id, context_type, zone_id, listened_at) \
             VALUES {}",
            paquet.join(", ")
        ));
    }
    sql
}

/// Les lignes rendues, en texte et triées : l'ordre de deux albums écoutés à
/// la même seconde n'est pas fixé par la requête, seul l'ensemble compte.
fn lignes_triees(db: &dyn DbBackend, sql: &str, params: &[&dyn ToSqlValue]) -> Vec<String> {
    let mut lignes: Vec<String> = db
        .query_many(sql, params)
        .unwrap_or_else(|e| panic!("requête en échec : {e}\n{sql}"))
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    lignes.sort();
    lignes
}

/// Les identifiants que `du_contexte` reçoit : des uniques (écoutés ou non,
/// dont un sans nombre de pistes), les deux homonymes d'une paire, un album
/// qui n'existe pas.
fn ids_de_contexte() -> Vec<i64> {
    let mut ids: Vec<i64> = (0..40).map(|i| PREMIER_UNIQUE + i).collect();
    ids.extend([PREMIER_HOMONYME, PREMIER_HOMONYME + 1, 999_999]);
    ids
}

/// Le cœur de la preuve, commun aux deux moteurs : ancienne et nouvelle
/// écritures rendent les mêmes lignes, avec et sans filtre de zone.
fn memes_lignes_qu_avant(db: &dyn DbBackend, engine: Engine) {
    let tout: i64 = 1_000_000;
    for zone in ["", "AND lh.zone_id = 2 "] {
        let avant = lignes_triees(
            db,
            &ancienne_deduits(engine, zone),
            &[&tout as &dyn ToSqlValue],
        );
        let apres = lignes_triees(
            db,
            &continue_listening_albums_deduits(engine, zone),
            &[&tout as &dyn ToSqlValue],
        );
        assert!(
            avant.len() > 20,
            "le banc doit donner de la matière ({zone:?}) : {} lignes",
            avant.len()
        );
        assert_eq!(
            apres, avant,
            "second rang : la réécriture UNION ALL ne rend pas les lignes de \
             l'ancienne jointure en OR (zone {zone:?})"
        );
    }

    let ids = ids_de_contexte();
    let avant = lignes_triees(db, &ancienne_du_contexte(&ids), &[]);
    let apres = lignes_triees(db, &continue_listening_albums_du_contexte(&ids), &[]);
    assert!(avant.len() > 20, "du_contexte : {} lignes", avant.len());
    assert_eq!(
        apres, avant,
        "albums du contexte : la réécriture ne rend pas les lignes d'avant"
    );
}

/// Le banc a bien la matière que la preuve prétend couvrir : des albums que
/// SEUL le repli rattache, et un homonyme que le repli doit laisser de côté.
fn le_banc_exerce_le_repli(db: &dyn DbBackend, engine: Engine) {
    let tout: i64 = 1_000_000;
    let ids: Vec<i64> = db
        .query_many(
            &continue_listening_albums_deduits(engine, ""),
            &[&tout as &dyn ToSqlValue],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(SqlValue::as_i64))
        .collect();
    let homonymes = |pair: i64| {
        ids.iter()
            .filter(|&&id| id >= PREMIER_HOMONYME && (id - PREMIER_HOMONYME) % 2 == pair)
            .count()
    };
    assert!(
        homonymes(0) > 0,
        "les « Live » d'Homonyme A ne sont rattachés que par le repli : ils \
         doivent remonter"
    );
    assert_eq!(
        homonymes(1),
        0,
        "les « Live » d'Homonyme B n'ont jamais été écoutés : aucun ne doit remonter"
    );
}

fn banc_sqlite(uniques: i64, ecoutes: i64) -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let mut sql = String::from("BEGIN;\n");
    for instruction in banc(uniques, ecoutes) {
        sql.push_str(&instruction);
        sql.push_str(";\n");
    }
    sql.push_str("COMMIT;");
    db.execute_batch(&sql).unwrap();
    db
}

/// Preuve (a), SQLite : mêmes lignes qu'avec l'ancienne jointure.
#[test]
fn le_union_all_rend_les_lignes_de_l_ancienne_jointure_sqlite() {
    let db = banc_sqlite(600, 4_000);
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    le_banc_exerce_le_repli(backend.as_ref(), Engine::Sqlite);
    memes_lignes_qu_avant(backend.as_ref(), Engine::Sqlite);
}

/// `HISTORIQUE_VERS_ALBUM` n'a pas bougé : les appelants qui la gardent
/// (genres les plus écoutés, albums non écoutés) suivent la même règle
/// qu'avant, et c'est elle que la nouvelle écriture doit reproduire.
#[test]
fn l_ancienne_jointure_reste_la_reference() {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        norm(super::home_queries::HISTORIQUE_VERS_ALBUM),
        norm(ANCIENNE_JOINTURE),
        "HISTORIQUE_VERS_ALBUM a changé de sens"
    );
}

/// La seconde branche ne rejoue plus de sous-requête corrélée par ligne :
/// c'est le `NOT EXISTS` des homonymes qui coûtait encore 4 s sur
/// PostgreSQL une fois le `OR` levé.
#[test]
fn le_repli_ne_rejoue_plus_de_sous_requete_par_ligne() {
    let sql = super::home_queries::historique_rattache_a_son_album("", None);
    assert!(
        !sql.contains("NOT EXISTS") && !sql.contains("= (SELECT"),
        "sous-requête corrélée revenue dans le repli. SQL :\n{sql}"
    );
}

/// Le second rang ne joint plus par un `OR` : c'est ce `OR` qui coûtait
/// 25,7 s sur PostgreSQL.
#[test]
fn le_second_rang_ne_joint_plus_par_un_or() {
    for engine in [Engine::Sqlite, Engine::Postgres] {
        for sql in [
            continue_listening_albums_deduits(engine, ""),
            continue_listening_albums_du_contexte(&[1, 2]),
        ] {
            assert!(
                !sql.contains("lh.album_id = a.id OR")
                    && !sql.contains(super::home_queries::HISTORIQUE_VERS_ALBUM),
                "la jointure en OR est revenue — 25,7 s sur PostgreSQL (fil 2130). \
                 SQL :\n{sql}"
            );
            assert!(sql.contains("UNION ALL"), "SQL :\n{sql}");
        }
    }
}

fn index_present(db: &SqliteDb) -> bool {
    let db: &dyn DbBackend = db;
    db.query_one(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'index' AND name = 'idx_listen_history_album_id' \
           AND tbl_name = 'listen_history'",
        &[],
    )
    .unwrap()
    .and_then(|r| r.first().and_then(SqlValue::as_i64))
        == Some(1)
}

fn version_max(db: &SqliteDb) -> i64 {
    let db: &dyn DbBackend = db;
    db.query_one("SELECT MAX(version) FROM _migrations", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(SqlValue::as_i64))
        .unwrap()
}

/// Preuve (b) : la 115 monte sur une base EXISTANTE — une base arrêtée avant
/// elle, avec de l'historique, sans l'index — et n'y perd rien.
#[test]
fn la_migration_115_monte_sur_une_base_existante() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    assert!(index_present(&db), "base neuve : l'index doit exister");
    assert_eq!(
        version_max(&db),
        i64::from(super::migrations::latest_version())
    );

    // Une base d'avant la 115 : sans l'index ni la ligne de version, avec de
    // l'historique.
    db.execute_batch(
        "DROP INDEX idx_listen_history_album_id;\n\
         DELETE FROM _migrations WHERE version >= 115;\n\
         INSERT INTO listen_history (title, album_title, listened_at) \
           VALUES ('Piste', 'Disque', '2026-10-01T10:00:00Z');",
    )
    .unwrap();
    assert!(!index_present(&db));
    assert!(version_max(&db) < 115);

    super::migrations::run_migrations(&db).unwrap();
    assert!(index_present(&db), "la 115 n'a pas posé l'index");
    assert_eq!(
        version_max(&db),
        i64::from(super::migrations::latest_version()),
        "la 115 n'est pas enregistrée"
    );
    let n = (&db as &dyn DbBackend)
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(SqlValue::as_i64));
    assert_eq!(n, Some(1), "l'historique a été touché");

    // La passe finale, elle, repose l'index sur une base déjà à jour qui l'a
    // perdu (restauration partielle, outil tiers).
    db.execute_batch("DROP INDEX idx_listen_history_album_id;")
        .unwrap();
    super::migrations::run_migrations(&db).unwrap();
    assert!(
        index_present(&db),
        "la passe finale doit reposer l'index même quand la 115 est enregistrée"
    );
}

// ─── PostgreSQL ────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use crate::db::backend::PostgresBackend;
    use std::time::Instant;

    async fn pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
        Some(sqlx::PgPool::connect(&url).await.unwrap_or_else(|e| {
            panic!("TUNE_TEST_PG_URL est posée ({url}) mais la connexion échoue : {e}")
        }))
    }

    fn vider(db: &Arc<dyn DbBackend>) {
        for table in ["listen_history", "albums", "artists", "zones"] {
            db.execute(
                &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
                &[],
            )
            .unwrap_or_else(|e| panic!("TRUNCATE {table} : {e}"));
        }
    }

    /// `ensure_schema` joué comme au démarrage : `listen_history.album_id`,
    /// `context_type` et l'index de la 079 n'arrivent que par là sur une base
    /// montée des seuls scripts numérotés (la base de `test-postgres.yml`).
    fn assurer_le_schema(db: &Arc<dyn DbBackend>) {
        for sql in crate::db::postgres::ENSURE_TABLES
            .iter()
            .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
        {
            let _ = db.execute(sql, &[]);
        }
    }

    fn charger(db: &Arc<dyn DbBackend>, uniques: i64, ecoutes: i64) {
        assurer_le_schema(db);
        vider(db);
        for instruction in banc(uniques, ecoutes) {
            db.execute(&instruction, &[])
                .unwrap_or_else(|e| panic!("banc : {e}\n{instruction}"));
        }
        db.execute("ANALYZE listen_history", &[]).unwrap();
        db.execute("ANALYZE albums", &[]).unwrap();
    }

    fn plan(db: &Arc<dyn DbBackend>, sql: &str) -> String {
        let tout: i64 = 1_000_000;
        db.query_many(
            &format!("EXPLAIN (ANALYZE, COSTS OFF) {sql}"),
            &[&tout as &dyn ToSqlValue],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(SqlValue::as_string))
        .collect::<Vec<_>>()
        .join("\n")
    }

    /// Preuve (a), PostgreSQL : mêmes lignes qu'avec l'ancienne jointure, sur
    /// le même banc que SQLite.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_2130_le_union_all_rend_les_lignes_de_l_ancienne_jointure() {
        let Some(pool) = pool().await else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));
        charger(&db, 600, 4_000);
        le_banc_exerce_le_repli(db.as_ref(), Engine::Postgres);
        memes_lignes_qu_avant(db.as_ref(), Engine::Postgres);
        vider(&db);
    }

    /// La mesure : plan et durée de l'ancienne et de la nouvelle écriture,
    /// sur un banc de quelques milliers d'albums. Les lignes sont comparées ;
    /// la nouvelle doit tenir dans la borne du widget (3 s), largement.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_2130_mesure_avant_apres() {
        let Some(pool) = pool().await else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));
        charger(&db, 6_000, 30_000);
        let tout: i64 = 1_000_000;

        let avant_sql = ancienne_deduits(Engine::Postgres, "");
        let apres_sql = continue_listening_albums_deduits(Engine::Postgres, "");
        let debut = Instant::now();
        let avant = lignes_triees(db.as_ref(), &avant_sql, &[&tout as &dyn ToSqlValue]);
        let duree_avant = debut.elapsed();
        let debut = Instant::now();
        let apres = lignes_triees(db.as_ref(), &apres_sql, &[&tout as &dyn ToSqlValue]);
        let duree_apres = debut.elapsed();
        eprintln!(
            "MESURE fil 2130 — {} albums, 30 000 écoutes, {} lignes rendues : \
             avant (OR) {duree_avant:?}, après (UNION ALL) {duree_apres:?}",
            6_000 + 2 * 1_200,
            apres.len()
        );
        eprintln!("PLAN AVANT :\n{}", plan(&db, &avant_sql));
        eprintln!("PLAN APRÈS :\n{}", plan(&db, &apres_sql));
        assert_eq!(apres, avant, "mêmes lignes sur le grand banc");
        assert!(
            duree_apres < std::time::Duration::from_secs(3),
            "la nouvelle écriture dépasse la borne du widget : {duree_apres:?}"
        );
        vider(&db);
    }

    /// Preuve (b), PostgreSQL : la 079 pose l'index sur la base existante, et
    /// se SAUTE sans erreur quand `listen_history.album_id` n'existe pas
    /// encore (base neuve : la colonne n'arrive que par `ENSURE_COLUMNS`).
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_2130_la_079_pose_l_index_et_se_saute_sans_colonne() {
        use sqlx::{Connection, Row};
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let (_, _, sql_079) = crate::db::migrations::PG_MIGRATIONS
            .iter()
            .find(|(v, _, _)| *v == 79)
            .expect("la 079 doit être inscrite");
        let mut c = sqlx::PgConnection::connect(&url).await.unwrap();
        let index_dans = |schema: &'static str| {
            format!(
                "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = '{schema}' \
                 AND indexname = 'idx_listen_history_album_id'"
            )
        };

        // Un schéma à part, où la table existe SANS la colonne.
        sqlx::raw_sql(
            "DROP SCHEMA IF EXISTS essai_2130 CASCADE; CREATE SCHEMA essai_2130; \
             SET search_path TO essai_2130; \
             CREATE TABLE schema_version (version INTEGER PRIMARY KEY, \
               applied_at TIMESTAMPTZ DEFAULT now(), name TEXT NOT NULL); \
             CREATE TABLE listen_history (id BIGSERIAL PRIMARY KEY, title TEXT);",
        )
        .execute(&mut c)
        .await
        .unwrap();
        sqlx::raw_sql(*sql_079)
            .execute(&mut c)
            .await
            .expect("sans la colonne, la 079 doit se sauter, pas échouer");
        let n: i64 = sqlx::query(sqlx::AssertSqlSafe(index_dans("essai_2130")))
            .fetch_one(&mut c)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0, "aucun index sans colonne");

        // La colonne arrive (ENSURE_COLUMNS) : la 079 rejouée pose l'index.
        sqlx::raw_sql("ALTER TABLE listen_history ADD COLUMN album_id BIGINT")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::raw_sql(*sql_079).execute(&mut c).await.unwrap();
        let n: i64 = sqlx::query(sqlx::AssertSqlSafe(index_dans("essai_2130")))
            .fetch_one(&mut c)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 1, "la 079 rejouée doit poser l'index");
        let version: i64 = sqlx::query("SELECT COUNT(*) FROM schema_version WHERE version = 79")
            .fetch_one(&mut c)
            .await
            .unwrap()
            .get(0);
        assert_eq!(version, 1, "la 079 doit s'inscrire, une fois");
        sqlx::raw_sql("SET search_path TO DEFAULT; DROP SCHEMA essai_2130 CASCADE;")
            .execute(&mut c)
            .await
            .unwrap();

        // La base de l'épreuve, EXISTANTE et peuplée de son schéma : la 079 y
        // pose l'index (ou l'y trouve), sans erreur.
        sqlx::raw_sql("DROP INDEX IF EXISTS idx_listen_history_album_id")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::raw_sql(*sql_079)
            .execute(&mut c)
            .await
            .expect("la 079 doit monter sur la base existante");
        let n: i64 = sqlx::query(
            "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = current_schema() \
             AND indexname = 'idx_listen_history_album_id'",
        )
        .fetch_one(&mut c)
        .await
        .unwrap()
        .get(0);
        assert_eq!(n, 1, "la 079 n'a pas posé l'index sur la base existante");
    }
}
