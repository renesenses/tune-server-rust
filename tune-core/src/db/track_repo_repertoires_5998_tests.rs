//! #5998 (Tades, fil 2185) — la vue Répertoires ne doit plus lire toute la
//! table pour ouvrir un dossier.
//!
//! Deux propriétés, sur les DEUX requêtes que `GET /library/browse/dir` exécute
//! ([`sql_pistes_du_dossier`] et [`sql_compter_pistes_par_sous_dossier`]) :
//!
//! 1. le PLAN part de `idx_tracks_chemin_dossier` et ne parcourt pas `tracks` ;
//! 2. le RÉSULTAT est celui de la même requête privée de sa plage, c'est-à-dire
//!    celui d'avant le correctif, sur les chemins piégés : casse ASCII (SQLite),
//!    dossier voisin au même début (`Album 1` / `Album 10` / `Album 1 bis`),
//!    dossier dont le nom suit le séparateur (`Album 10` contre `Album 1/`, et
//!    `Album 1` + le caractère suivant le séparateur), jokers `%` et `_`,
//!    piste découpée par une feuille CUE (`file_path` NUL).

use super::*;

fn sep() -> char {
    std::path::MAIN_SEPARATOR
}

/// `["m", "A"]` → `/m/A` (ou `\m\A` sous Windows).
fn chemin(parts: &[&str]) -> String {
    let s = sep().to_string();
    format!("{s}{}", parts.join(&s))
}

/// Le séparateur suivant (`/` → `0`) : un dossier dont le nom l'emploie se
/// range juste APRÈS la plage du dossier voisin, le cas limite de la borne.
fn suivant_du_separateur() -> String {
    char::from(sep() as u8 + 1).to_string()
}

/// La requête telle qu'avant #5998 : la même, sans la plage.
fn sans_la_plage(engine: Engine, sql: &str, expr: &str, p3: &str, p4: &str) -> String {
    let plage = plage_du_dossier(engine, expr, p3, p4);
    let sans = sql.replace(&format!("{plage} AND "), "");
    assert_ne!(sans, sql, "la plage doit figurer dans la requête");
    sans
}

/// Les chemins piégés, posés sous `/m/`, et le dossier qu'on ouvre.
fn chemins_pieges() -> Vec<(Option<String>, Option<String>)> {
    let suivant = suivant_du_separateur();
    let mut v: Vec<(Option<String>, Option<String>)> = Vec::new();
    for n in 1..=14 {
        v.push((
            Some(chemin(&["m", "Album 1", &format!("{n:02}.flac")])),
            None,
        ));
    }
    for p in [
        chemin(&["m", "Album 1", "CD2", "01.flac"]),
        chemin(&["m", "Album 1", "CD2", "02.flac"]),
        chemin(&["m", "Album 1", "Bonus", "01.flac"]),
        chemin(&["m", "ALBUM 1", "01.flac"]),
        chemin(&["m", "album 1", "CD3", "01.flac"]),
        // Sous Unix le caractère qui suit `/` est `0` : « Album 10 » est le
        // dossier même de la borne haute (exclue), d'où le second fichier.
        chemin(&["m", "Album 10", "02.flac"]),
        chemin(&["m", "Album 1 bis", "01.flac"]),
        chemin(&["m", &format!("Album 1{suivant}"), "01.flac"]),
        chemin(&["m", "Album_1", "01.flac"]),
        chemin(&["m", "Album%1", "01.flac"]),
        chemin(&["m", "Album 1.flac"]),
        chemin(&["n", "Album 1", "01.flac"]),
    ] {
        v.push((Some(p), None));
    }
    // Tranche CUE : `file_path` NUL, le fichier dans `cue_media_path`.
    v.push((None, Some(chemin(&["m", "Album 1", "image.flac"]))));
    v.push((None, Some(chemin(&["m", "Album 1", "CD4", "image.flac"]))));
    v
}

fn dossier_ouvert() -> String {
    chemin(&["m", "Album 1"])
}

/// Les paramètres que la route lie, dans l'ordre : motif, séparateur, bornes.
fn parametres(dossier: &str) -> (String, String, String, String) {
    let (bas, haut) = bornes_du_dossier(dossier);
    (folder_like_pattern(dossier), sep().to_string(), bas, haut)
}

fn depart(dossier: &str) -> usize {
    dossier.trim_end_matches(['/', '\\']).chars().count() + sep().to_string().chars().count() + 1
}

fn lignes(db: &dyn DbBackend, sql: &str, p: &(String, String, String, String)) -> Vec<String> {
    let params: [&dyn ToSqlValue; 4] = [&p.0, &p.1, &p.2, &p.3];
    // Sans la plage, la requête ne lit que le motif et le séparateur : SQLite
    // refuse les paramètres en trop.
    let n = if sql.contains("?3") || sql.contains("$3") {
        4
    } else {
        2
    };
    let mut v: Vec<String> = db
        .query_many(sql, &params[..n])
        .unwrap()
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| format!("{c:?}"))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    v.sort();
    v
}

/// Compare chacune des deux requêtes de la route à sa version sans plage.
fn memes_resultats_qu_avant(db: &dyn DbBackend, engine: Engine) {
    let (p3, p4) = match engine {
        Engine::Postgres => ("$3", "$4"),
        Engine::Sqlite => ("?3", "?4"),
    };
    for dossier in [
        dossier_ouvert(),
        chemin(&["m"]),
        chemin(&["m", "Album 1", "CD2"]),
        chemin(&["m", "Album_1"]),
        chemin(&["m", "album 1"]),
    ] {
        let p = parametres(&dossier);
        let pistes = sql_pistes_du_dossier(engine, depart(&dossier));
        let pistes_avant = sans_la_plage(
            engine,
            &pistes,
            "COALESCE(t.file_path, t.cue_media_path)",
            p3,
            p4,
        );
        let comptes = sql_compter_pistes_par_sous_dossier(engine, depart(&dossier));
        let comptes_avant = sans_la_plage(engine, &comptes, CHEMIN_DE_LA_PISTE, p3, p4);
        let (apres, avant) = (lignes(db, &pistes, &p), lignes(db, &pistes_avant, &p));
        assert_eq!(
            apres, avant,
            "{engine:?} : les pistes de {dossier:?} ont changé avec la plage"
        );
        let (apres_c, avant_c) = (lignes(db, &comptes, &p), lignes(db, &comptes_avant, &p));
        assert_eq!(
            apres_c, avant_c,
            "{engine:?} : les comptes de {dossier:?} ont changé avec la plage"
        );
        if dossier == dossier_ouvert() {
            // 14 pistes + la tranche CUE (+ « ALBUM 1/01.flac » sur SQLite,
            // dont `LIKE` ignore la casse ASCII — comportement d'avant gardé).
            let attendu = if engine == Engine::Sqlite { 16 } else { 15 };
            assert_eq!(apres.len(), attendu, "{engine:?} : {apres:#?}");
        }
    }
}

/// Le plan SQLite de `sql` : les lignes `detail` d'`EXPLAIN QUERY PLAN`.
fn plan_sqlite(db: &SqliteDb, sql: &str, p: &(String, String, String, String)) -> Vec<String> {
    let params: [&dyn ToSqlValue; 4] = [&p.0, &p.1, &p.2, &p.3];
    let n = if sql.contains("?3") { 4 } else { 2 };
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &params[..n])
        .unwrap()
        .iter()
        .filter_map(|r| r.get(3).and_then(|v| v.as_string()))
        .collect()
}

fn base_sqlite() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    // La passe finale des migrations, celle du démarrage réel, pose l'index.
    crate::db::migrations::run_migrations(&db).unwrap();
    db
}

fn inserer(db: &dyn DbBackend, pistes: &[(Option<String>, Option<String>)]) {
    for (i, (fp, cue)) in pistes.iter().enumerate() {
        let titre = format!("Piste {i}");
        let n = (i % 14) as i64 + 1;
        db.execute(
            "INSERT INTO tracks (title, file_path, cue_media_path, format, source, track_number) \
             VALUES (?, ?, ?, 'flac', 'local', ?)",
            &[&titre as &dyn ToSqlValue, fp, cue, &n],
        )
        .unwrap();
    }
}

#[test]
fn sqlite_les_requetes_des_repertoires_partent_de_l_index_5998() {
    let db = base_sqlite();
    let s = sep();
    db.execute_batch(&format!(
        "WITH RECURSIVE g(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < 20000) \
         INSERT INTO tracks (title, file_path, format, source, track_number) \
         SELECT 'Fond ' || i, '{s}fond{s}Artiste ' || (i / 200) || '{s}Album ' || (i / 14) || '{s}' || i || '.flac', \
         'flac', 'local', i % 14 FROM g; ANALYZE;"
    ))
    .unwrap();
    inserer(&db, &chemins_pieges());
    let dossier = dossier_ouvert();
    let p = parametres(&dossier);
    for (nom, sql) in [
        (
            "pistes",
            sql_pistes_du_dossier(Engine::Sqlite, depart(&dossier)),
        ),
        (
            "comptes",
            sql_compter_pistes_par_sous_dossier(Engine::Sqlite, depart(&dossier)),
        ),
    ] {
        let plan = plan_sqlite(&db, &sql, &p);
        eprintln!("#5998 plan SQLite {nom} : {plan:?}");
        assert!(
            plan.iter()
                .any(|l| l.contains("USING INDEX idx_tracks_chemin_dossier")),
            "#5998 : la requête « {nom} » de la vue Répertoires ne part pas de \
             idx_tracks_chemin_dossier — elle relit toute la table : {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|l| l.starts_with("SCAN t ") || l == "SCAN t" || l.starts_with("SCAN tracks")),
            "#5998 : la requête « {nom} » parcourt toute la table des pistes : {plan:?}"
        );
    }
}

#[test]
fn sqlite_la_plage_ne_change_aucun_resultat_5998() {
    let db = base_sqlite();
    inserer(&db, &chemins_pieges());
    memes_resultats_qu_avant(&db, Engine::Sqlite);
}

#[test]
fn les_bornes_encadrent_exactement_le_prefixe_5998() {
    let s = sep();
    let (bas, haut) = bornes_du_dossier(&format!("{s}m{s}Album 1{s}"));
    assert_eq!(bas, format!("{s}m{s}Album 1{s}"));
    assert_eq!(haut, format!("{s}m{s}Album 1{}", suivant_du_separateur()));
    // Le chemin du dossier lui-même n'est pas dans la plage, ses enfants oui.
    let dedans = format!("{s}m{s}Album 1{s}01.flac");
    assert!(bas.as_str() <= dedans.as_str() && dedans.as_str() < haut.as_str());
    let voisin = format!("{s}m{s}Album 10{s}01.flac");
    assert!(!(bas.as_str() <= voisin.as_str() && voisin.as_str() < haut.as_str()));
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use crate::db::backend::PostgresBackend;

    /// Une bibliothèque de fond : 20 000 pistes, 1 400 albums, hors de `/m/`.
    fn sql_fond(n: i64) -> String {
        let s = sep();
        format!(
            "INSERT INTO tracks (title, file_path, format, source, track_number) \
             SELECT 'Fond ' || i, '{s}fond{s}Artiste ' || (i / 200) || '{s}Album ' || (i / 14) || '{s}' || i || '.flac', \
             'flac', 'local', i % 14 FROM generate_series(1, {n}) AS g(i)"
        )
    }

    /// Le plan PG de `sql`, sur UNE connexion. `generique` force le plan
    /// GÉNÉRIQUE d'une requête préparée (`PREPARE` + `EXPLAIN EXECUTE`), celui
    /// que PostgreSQL finit par garder pour une requête rejouée — sqlx garde
    /// ses requêtes préparées. Les paramètres y sont inconnus au moment du
    /// plan : un `LIKE $1` seul ne peut pas y partir de l'index, la plage oui.
    /// Un `EXPLAIN` ordinaire, lui, planifie avec les valeurs liées.
    async fn plan_pg(
        pool: &sqlx::PgPool,
        sql: &str,
        p: &(String, String, String, String),
        generique: bool,
    ) -> Vec<String> {
        use sqlx::Row as _;
        let mut c = pool.acquire().await.unwrap();
        let lignes = if generique {
            let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "SET plan_cache_mode = force_generic_plan; \
                 PREPARE q5998(text, text, text, text) AS {sql}"
            )))
            .execute(&mut *c)
            .await
            .unwrap();
            let r = sqlx::query(sqlx::AssertSqlSafe(format!(
                "EXPLAIN EXECUTE q5998({}, {}, {}, {})",
                lit(&p.0),
                lit(&p.1),
                lit(&p.2),
                lit(&p.3)
            )))
            .fetch_all(&mut *c)
            .await
            .unwrap();
            sqlx::raw_sql("DEALLOCATE q5998; RESET plan_cache_mode")
                .execute(&mut *c)
                .await
                .unwrap();
            r
        } else {
            let mut q = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
                .bind(&p.0)
                .bind(&p.1);
            if sql.contains("$3") {
                q = q.bind(&p.2).bind(&p.3);
            }
            q.fetch_all(&mut *c).await.unwrap()
        };
        lignes.iter().map(|r| r.get::<String, _>(0)).collect()
    }

    /// Saute sans `TUNE_TEST_PG_URL`. Passe par le VRAI démarrage
    /// (`run_pg_migrations`), qui doit poser l'index lui-même : il est retiré
    /// d'abord, pour qu'un index laissé par un passage précédent ne réponde
    /// pas à la place de la passe.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_5998_repertoires_par_l_index_et_memes_resultats() {
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
        sqlx::raw_sql("DROP INDEX IF EXISTS idx_tracks_chemin_dossier")
            .execute(&pool)
            .await
            .unwrap();
        crate::db::migrations::run_pg_migrations(&pool)
            .await
            .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool.clone()));
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();
        db.execute(&sql_fond(20_000), &[]).unwrap();
        inserer(db.as_ref(), &chemins_pieges());
        db.execute("ANALYZE tracks", &[]).unwrap();

        memes_resultats_qu_avant(db.as_ref(), Engine::Postgres);

        let dossier = dossier_ouvert();
        let p = parametres(&dossier);
        for (nom, sql) in [
            (
                "pistes",
                sql_pistes_du_dossier(Engine::Postgres, depart(&dossier)),
            ),
            (
                "comptes",
                sql_compter_pistes_par_sous_dossier(Engine::Postgres, depart(&dossier)),
            ),
        ] {
            for generique in [false, true] {
                let plan = plan_pg(&pool, &sql, &p, generique).await;
                eprintln!("#5998 plan PG {nom} (générique : {generique}) : {plan:#?}");
                assert!(
                    plan.iter().any(|l| l.contains("idx_tracks_chemin_dossier")),
                    "#5998 : la requête « {nom} » ne part pas de idx_tracks_chemin_dossier \
                     sur PostgreSQL (plan générique : {generique}) : {plan:#?}"
                );
                assert!(
                    !plan.iter().any(|l| l.contains("Seq Scan on tracks")),
                    "#5998 : la requête « {nom} » parcourt toute la table sur PostgreSQL \
                     (plan générique : {generique}) : {plan:#?}"
                );
            }
        }
    }
}
