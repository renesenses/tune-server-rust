//! #8416 (forum 2626) — les écoutes IMPORTÉES (`imported_plays`, #7763)
//! comptaient dans la Bibliothèque (`HistoryRepo::plays_for_tracks`) mais pas
//! dans les règles « nombre d'écoutes » et « dernière écoute » des listes et
//! collections intelligentes : les fragments partagés
//! [`sql::ecoutes_de_la_piste`] et [`sql::derniere_ecoute_de_la_piste`] ne
//! lisaient que `listen_history`.
//!
//! Témoin de l'issue, sur SQLite ET PostgreSQL (`pg_8416_…`, saute sans
//! `TUNE_TEST_PG_URL`) :
//!
//! - piste A : 3 écoutes importées datées, aucune dans l'historique ;
//! - piste B : 1 écoute d'historique + 2 importées sans date (liste m3u) ;
//! - piste C : jamais écoutée.
//!
//! A est seule sur l'album 1, B et C sur l'album 2.
//!
//! Contre-épreuve : remettre dans `ecoutes_de_la_piste` le seul
//! `COUNT(*) FROM listen_history` fait rougir le témoin (A vaut 0 au lieu de
//! 3, comme sur la rc11).

use super::*;
use crate::db::ecoutes_importees::EcoutesImporteesRepo;
use crate::db::migrations;

const INTERPRETE: &str = "Interprète 8416";

/// Les trois pistes A, B, C (`base`, `base + 1`, `base + 2`) et leurs deux
/// albums (`base`, `base + 1`), puis les écoutes du témoin, écrites par les
/// vrais chemins : `record` pour l'historique, `ajouter` pour l'import.
fn poser_le_temoin(db: &Arc<dyn DbBackend>, base: i64) {
    db.execute(
        &format!("INSERT INTO artists (id, name) VALUES ({base}, '{INTERPRETE}')"),
        &[],
    )
    .unwrap();
    for album in [base, base + 1] {
        db.execute(
            &format!(
                "INSERT INTO albums (id, title, artist_id, track_count) \
                 VALUES ({album}, 'Album {album}', {base}, 1)"
            ),
            &[],
        )
        .unwrap();
    }
    for (decalage, album) in [(0, base), (1, base + 1), (2, base + 1)] {
        let id = base + decalage;
        db.execute(
            &format!(
                "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
                 VALUES ({id}, 'Titre {id}', {base}, {album}, {}, '/musique/{id}.flac')",
                decalage + 1
            ),
            &[],
        )
        .unwrap();
    }
    let repo = HistoryRepo::with_backend(db.clone());
    let id = repo
        .record(&ListenRecord {
            title: format!("Titre {}", base + 1),
            artist_name: Some(INTERPRETE.into()),
            source: "local".into(),
            duration_ms: 240_000,
            ..Default::default()
        })
        .unwrap();
    let sql = match db.engine() {
        Engine::Sqlite => "UPDATE listen_history SET listened_at = ? WHERE id = ?",
        Engine::Postgres => "UPDATE listen_history SET listened_at = $1 WHERE id = $2",
    };
    db.execute(sql, &[&"2026-10-01T09:00:00Z" as &dyn ToSqlValue, &id])
        .unwrap();
    let import = EcoutesImporteesRepo::with_backend(db.clone());
    import
        .ajouter(base, 3, Some("2026-11-02T10:00:00Z"))
        .unwrap();
    import.ajouter(base + 1, 2, None).unwrap();
}

/// Les pistes du témoin que retient la condition `cond` (alias `t`).
fn pistes(db: &Arc<dyn DbBackend>, base: i64, cond: &str) -> Vec<i64> {
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE t.id BETWEEN {base} AND {} AND ({cond}) ORDER BY t.id",
        base + 2
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap() - base)
        .collect()
}

/// Les albums du témoin (0 ou 1) que retient la condition `cond` (alias `al`).
fn albums(db: &Arc<dyn DbBackend>, base: i64, cond: &str) -> Vec<i64> {
    let sql = format!(
        "SELECT al.id FROM albums al WHERE al.id BETWEEN {base} AND {} AND ({cond}) ORDER BY al.id",
        base + 1
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap() - base)
        .collect()
}

fn verifier(db: &Arc<dyn DbBackend>, base: i64) {
    let (a, b, c) = (base, base + 1, base + 2);
    let date = |d: &str| Some(d.to_string());
    let compte = sql::ecoutes_de_la_piste("t");
    let derniere = sql::derniere_ecoute_de_la_piste("t");

    // 1. Le nombre et la date des règles sont ceux de la Bibliothèque.
    let bibliotheque = HistoryRepo::with_backend(db.clone())
        .plays_for_tracks(&[a, b, c])
        .unwrap();
    assert_eq!(bibliotheque[&a], (3, date("2026-11-02T10:00:00Z")));
    assert_eq!(bibliotheque[&b], (3, date("2026-10-01T09:00:00Z")));
    assert_eq!(bibliotheque[&c], (0, None));
    for piste in [a, b, c] {
        let ligne = db
            .query_one(
                &format!("SELECT {compte}, {derniere} FROM tracks t WHERE t.id = {piste}"),
                &[],
            )
            .unwrap()
            .unwrap();
        let regles = (
            ligne[0].as_i64().unwrap(),
            ligne.get(1).and_then(|v| v.as_string()),
        );
        assert_eq!(
            regles,
            bibliotheque[&piste],
            "#8416 : les règles intelligentes doivent compter comme la Bibliothèque \
             (piste {})",
            piste - base
        );
    }

    // 2. Les règles « nombre d'écoutes » à la piste.
    assert_eq!(
        pistes(db, base, &format!("{compte} = 0")),
        vec![2],
        "= 0 → C seule"
    );
    assert_eq!(
        pistes(db, base, &format!("{compte} > 2")),
        vec![0, 1],
        "> 2 → A et B"
    );
    assert_eq!(
        pistes(db, base, &format!("{compte} = 3")),
        vec![0, 1],
        "= 3 → A et B"
    );

    // 3. « Dernière écoute » : « jamais » → C seule (B a une écoute sans
    //    date importée, mais une d'historique) ; « après le 15/10 » → A.
    assert_eq!(
        pistes(db, base, &format!("({derniere} IS NULL AND {compte} = 0)")),
        vec![2],
        "jamais écoutée → C seule"
    );
    assert_eq!(
        pistes(db, base, &format!("{derniere} > '2026-10-15'")),
        vec![0],
        "dernière écoute importée"
    );

    // 4. Le tri par nombre d'écoutes range A et B (3) avant C (0).
    let tri = db
        .query_many(
            &format!(
                "SELECT t.id FROM tracks t WHERE t.id BETWEEN {a} AND {c} \
                 ORDER BY {compte} DESC, t.id"
            ),
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r[0].as_i64().unwrap() - base)
        .collect::<Vec<_>>();
    assert_eq!(tri, vec![0, 1, 2]);

    // 5. Les albums : album 0 (A) = 3, album 1 (B + C) = 3.
    let ea = sql::ecoutes_des_albums();
    let parmi = |cond: &str| {
        albums(
            db,
            base,
            &format!("al.id IN (SELECT ea.album_id FROM {ea} ea WHERE {cond})"),
        )
    };
    let hors = |cond: &str| {
        albums(
            db,
            base,
            &format!("al.id NOT IN (SELECT ea.album_id FROM {ea} ea WHERE {cond})"),
        )
    };
    assert_eq!(parmi("ea.n = 3"), vec![0, 1], "albums : = 3");
    assert!(hors("ea.n > 0").is_empty(), "albums : aucun n'est à 0");
    assert_eq!(
        parmi("ea.derniere > '2026-10-15'"),
        vec![0],
        "albums : dernière écoute importée"
    );
    assert_eq!(parmi("ea.derniere < '2026-10-15'"), vec![1]);

    // 6. Une écoute importée sans date sur C : C n'est plus « jamais
    //    écoutée », et l'album 1 passe à 4.
    EcoutesImporteesRepo::with_backend(db.clone())
        .ajouter(c, 1, None)
        .unwrap();
    assert!(
        pistes(db, base, &format!("({derniere} IS NULL AND {compte} = 0)")).is_empty(),
        "une écoute importée sans date reste une écoute"
    );
    assert_eq!(parmi("ea.n = 4"), vec![1]);
}

#[test]
fn sqlite_les_regles_intelligentes_comptent_les_ecoutes_importees_8416() {
    let base = SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    poser_le_temoin(&db, 1);
    verifier(&db, 1);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8416_les_regles_intelligentes_comptent_les_ecoutes_importees() {
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
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let nettoyer = |db: &Arc<dyn DbBackend>| {
        for sql in [
            format!("DELETE FROM listen_history WHERE artist_name = '{INTERPRETE}'"),
            "DELETE FROM imported_plays WHERE track_id BETWEEN 8416 AND 8418".to_string(),
            "DELETE FROM tracks WHERE id BETWEEN 8416 AND 8418".to_string(),
            "DELETE FROM albums WHERE id BETWEEN 8416 AND 8417".to_string(),
            "DELETE FROM artists WHERE id = 8416".to_string(),
        ] {
            db.execute(&sql, &[]).unwrap();
        }
    };
    nettoyer(&db);
    poser_le_temoin(&db, 8416);
    verifier(&db, 8416);
    nettoyer(&db);
}
