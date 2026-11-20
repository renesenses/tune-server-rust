//! #9116 (fil 2935) — le compte d'une racine de la vue Répertoires
//! (`GET /library/browse`) ne doit plus parcourir toute la table.
//!
//! Deux propriétés de [`sql_compter_pistes_sous_dossier`] :
//!
//! 1. le PLAN ne lit pas les lignes de `tracks` : index couvrant de
//!    `file_path` sur SQLite, plage de `idx_tracks_chemin_dossier` sur
//!    PostgreSQL (avec `TUNE_TEST_PG_URL`), y compris en plan générique ;
//! 2. le RÉSULTAT est celui de l'ancienne requête de la route, à l'unité près,
//!    sur des chemins piégés (voisins, casse, jokers, tranche CUE).

use super::*;

fn s() -> char {
    std::path::MAIN_SEPARATOR
}

fn chemin(parts: &[&str]) -> String {
    let s = s().to_string();
    format!("{s}{}", parts.join(&s))
}

/// Des chemins piégés : voisins au même début, casse, jokers, tranche CUE.
fn pieges() -> Vec<(Option<String>, Option<String>)> {
    let suivant = char::from(s() as u8 + 1).to_string();
    let mut v: Vec<(Option<String>, Option<String>)> = Vec::new();
    for p in [
        chemin(&["musique", "A", "01.flac"]),
        chemin(&["musique", "A", "B", "02.flac"]),
        chemin(&["MUSIQUE", "C", "03.flac"]),
        chemin(&["musique2", "D", "04.flac"]),
        chemin(&["musique ", "E", "05.flac"]),
        chemin(&[&format!("musique{suivant}"), "F", "06.flac"]),
        chemin(&["musiqu_", "G", "07.flac"]),
        chemin(&["musique%", "H", "08.flac"]),
        chemin(&["musique.flac"]),
    ] {
        v.push((Some(p), None));
    }
    v.push((None, Some(chemin(&["musique", "A", "image.flac"]))));
    v
}

fn inserer(db: &dyn DbBackend, pistes: &[(Option<String>, Option<String>)]) {
    for (i, (fp, cue)) in pistes.iter().enumerate() {
        let titre = format!("Piste {i}");
        db.execute(
            "INSERT INTO tracks (title, file_path, cue_media_path, format, source) \
             VALUES (?, ?, ?, 'flac', 'local')",
            &[&titre as &dyn ToSqlValue, fp, cue],
        )
        .unwrap();
    }
}

fn compter(db: &dyn DbBackend, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    db.query_one(sql, params)
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
}

/// L'ancienne requête de la route, et le même compte par la nouvelle.
fn comparer(db: &dyn DbBackend, engine: Engine) {
    let p1 = match engine {
        Engine::Postgres => "$1",
        Engine::Sqlite => "?1",
    };
    let ancienne = format!(
        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE {p1}{}",
        like_escape_clause()
    );
    for dossier in [
        chemin(&["musique"]),
        format!("{}{}", chemin(&["musique"]), s()),
        chemin(&["musique", "A"]),
        chemin(&["musiqu_"]),
        chemin(&["musique%"]),
        chemin(&["fond"]),
    ] {
        let motif = folder_like_pattern(&dossier);
        let apres = compter_pistes_sous_dossier(db, &dossier).unwrap();
        let avant = compter(db, &ancienne, &[&motif as &dyn ToSqlValue]);
        assert_eq!(
            apres, avant,
            "{engine:?} : le compte de {dossier:?} a changé"
        );
    }
    // Racine piégée : 2 fichiers, et sur SQLite (LIKE ignore la casse ASCII)
    // « MUSIQUE/C » en plus ; la tranche CUE n'est pas comptée, comme avant.
    let attendu = if engine == Engine::Sqlite { 3 } else { 2 };
    assert_eq!(
        compter_pistes_sous_dossier(db, &chemin(&["musique"])).unwrap(),
        attendu
    );
}

fn base_sqlite() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db
}

#[test]
fn sqlite_le_compte_d_une_racine_ne_lit_que_l_index_9116() {
    let db = base_sqlite();
    let sep = s();
    db.execute_batch(&format!(
        "WITH RECURSIVE g(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < 20000) \
         INSERT INTO tracks (title, file_path, format, source) \
         SELECT 'Fond ' || i, '{sep}fond{sep}Artiste ' || (i / 200) || '{sep}Album ' || (i / 14) \
         || '{sep}' || i || '.flac', 'flac', 'local' FROM g; ANALYZE;"
    ))
    .unwrap();
    inserer(&db, &pieges());
    let motif = folder_like_pattern(&chemin(&["musique"]));
    let plan: Vec<String> = db
        .query_many(
            &format!(
                "EXPLAIN QUERY PLAN {}",
                sql_compter_pistes_sous_dossier(Engine::Sqlite)
            ),
            &[&motif as &dyn ToSqlValue],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.get(3).and_then(|v| v.as_string()))
        .collect();
    eprintln!("#9116 plan SQLite du compte de racine : {plan:?}");
    assert!(
        !plan.is_empty() && plan.iter().all(|l| l.contains("COVERING INDEX")),
        "#9116 : le compte d'une racine doit se lire dans un index couvrant, \
         sans relire les lignes de `tracks` : {plan:?}"
    );
    comparer(&db, Engine::Sqlite);
}

#[test]
fn sqlite_le_compte_d_une_racine_ne_change_pas_9116() {
    let db = base_sqlite();
    inserer(&db, &pieges());
    comparer(&db, Engine::Sqlite);
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use crate::db::backend::PostgresBackend;

    async fn plan_pg(
        pool: &sqlx::PgPool,
        sql: &str,
        p: &[String; 3],
        generique: bool,
    ) -> Vec<String> {
        use sqlx::Row as _;
        let mut c = pool.acquire().await.unwrap();
        let lignes = if generique {
            let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "SET plan_cache_mode = force_generic_plan; \
                 PREPARE q9116(text, text, text) AS {sql}"
            )))
            .execute(&mut *c)
            .await
            .unwrap();
            let r = sqlx::query(sqlx::AssertSqlSafe(format!(
                "EXPLAIN EXECUTE q9116({}, {}, {})",
                lit(&p[0]),
                lit(&p[1]),
                lit(&p[2])
            )))
            .fetch_all(&mut *c)
            .await
            .unwrap();
            sqlx::raw_sql("DEALLOCATE q9116; RESET plan_cache_mode")
                .execute(&mut *c)
                .await
                .unwrap();
            r
        } else {
            sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
                .bind(&p[0])
                .bind(&p[1])
                .bind(&p[2])
                .fetch_all(&mut *c)
                .await
                .unwrap()
        };
        lignes.iter().map(|r| r.get::<String, _>(0)).collect()
    }

    /// Saute sans `TUNE_TEST_PG_URL`. Passe par le vrai démarrage, qui pose
    /// l'index lui-même.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_9116_compte_de_racine_par_l_index_et_memes_resultats() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
        for sql in crate::db::postgres::ENSURE_TABLES
            .iter()
            .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
        {
            let _ = sqlx::raw_sql(*sql).execute(&pool).await;
        }
        crate::db::migrations::run_pg_migrations(&pool)
            .await
            .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool.clone()));
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();
        let sep = s();
        db.execute(
            &format!(
                "INSERT INTO tracks (title, file_path, format, source) \
                 SELECT 'Fond ' || i, '{sep}fond{sep}Artiste ' || (i / 200) || '{sep}Album ' \
                 || (i / 14) || '{sep}' || i || '.flac', 'flac', 'local' \
                 FROM generate_series(1, 20000) AS g(i)"
            ),
            &[],
        )
        .unwrap();
        inserer(db.as_ref(), &pieges());
        db.execute("ANALYZE tracks", &[]).unwrap();

        comparer(db.as_ref(), Engine::Postgres);

        let dossier = chemin(&["musique"]);
        let (bas, haut) = bornes_du_dossier(&dossier);
        let p = [folder_like_pattern(&dossier), bas, haut];
        let sql = sql_compter_pistes_sous_dossier(Engine::Postgres);
        for generique in [false, true] {
            let plan = plan_pg(&pool, &sql, &p, generique).await;
            eprintln!("#9116 plan PG (générique : {generique}) : {plan:#?}");
            assert!(
                plan.iter().any(|l| l.contains("idx_tracks_chemin_dossier")),
                "#9116 : le compte de racine ne part pas de l'index sur PostgreSQL \
                 (générique : {generique}) : {plan:#?}"
            );
            assert!(
                !plan.iter().any(|l| l.contains("Seq Scan on tracks")),
                "#9116 : le compte de racine parcourt la table sur PostgreSQL \
                 (générique : {generique}) : {plan:#?}"
            );
        }
    }
}
