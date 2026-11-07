//! #8062 — dans une playlist de service (Qobuz…), les colonnes « # Plays » et
//! « Last played » restaient vides : aucune route ne joignait l'historique aux
//! pistes de service.
//!
//! Une écoute de service s'écrit `source` = le service et `source_id` = son
//! identifiant chez lui. Ces témoins tiennent la règle de
//! [`HistoryRepo::ecoutes_de_service_par_identifiant`] sur SQLite ET sur
//! PostgreSQL (`pg_8062_…`, saute sans `TUNE_TEST_PG_URL`) :
//!
//! - une piste Qobuz écoutée deux fois par le profil 1 → `(2, Some(dernière))` ;
//! - une piste jamais jouée → `(0, None)` ;
//! - le profil 2 ne voit pas les écoutes du profil 1 ;
//! - une écoute sans profil compte pour tous ;
//! - le même identifiant chez un AUTRE service ne compte pas.
//!
//! Contre-épreuve : retirer la condition de profil de
//! `sql::ecoutes_de_service_par_identifiant` fait tomber le témoin du profil 2.

use super::*;
use crate::db::migrations;

fn ecoute(source: &str, source_id: &str, profil: Option<i64>, quand: &str) -> ListenRecord {
    ListenRecord {
        title: format!("Titre {source_id}"),
        artist_name: Some("Interprète 8062".into()),
        source: source.into(),
        source_id: Some(source_id.into()),
        duration_ms: 240_000,
        profile_id: profil,
        listened_at: Some(quand.into()),
        ..Default::default()
    }
}

/// `record` laisse la base dater l'écoute : la date voulue est reposée
/// ensuite, pour que « la dernière » soit vérifiable.
fn enregistrer(repo: &HistoryRepo, db: &Arc<dyn DbBackend>, rec: &ListenRecord) {
    let id = repo.record(rec).unwrap();
    let quand = rec.listened_at.clone().unwrap();
    let sql = match db.engine() {
        Engine::Sqlite => "UPDATE listen_history SET listened_at = ? WHERE id = ?",
        Engine::Postgres => "UPDATE listen_history SET listened_at = $1 WHERE id = $2",
    };
    db.execute(sql, &[&quand as &dyn ToSqlValue, &id]).unwrap();
}

fn jeu_d_ecoutes(db: &Arc<dyn DbBackend>) -> HistoryRepo {
    let repo = HistoryRepo::with_backend(db.clone());
    enregistrer(
        &repo,
        db,
        &ecoute("qobuz", "8062001", Some(1), "2026-11-01T10:00:00Z"),
    );
    enregistrer(
        &repo,
        db,
        &ecoute("qobuz", "8062001", Some(1), "2026-11-05T21:30:00Z"),
    );
    // Un autre profil écoute une autre piste : le profil 1 ne la voit pas.
    enregistrer(
        &repo,
        db,
        &ecoute("qobuz", "8062003", Some(2), "2026-11-02T08:00:00Z"),
    );
    // Une écoute sans profil : elle compte pour tous.
    enregistrer(
        &repo,
        db,
        &ecoute("qobuz", "8062004", None, "2026-11-03T12:00:00Z"),
    );
    // Le même identifiant chez TIDAL n'est pas la piste Qobuz.
    enregistrer(
        &repo,
        db,
        &ecoute("tidal", "8062001", Some(1), "2026-11-06T09:00:00Z"),
    );
    repo
}

fn verifier(repo: &HistoryRepo) {
    let ids = ["8062001", "8062002", "8062003", "8062004"];
    let p1 = repo
        .ecoutes_de_service_par_identifiant("qobuz", &ids, 1)
        .unwrap();
    assert_eq!(
        p1.get("8062001"),
        Some(&(2, Some("2026-11-05T21:30:00Z".to_string()))),
        "#8062 : une piste Qobuz écoutée deux fois doit rendre 2 et sa dernière écoute — \
         l'écoute TIDAL de même identifiant ne compte pas"
    );
    assert_eq!(
        p1.get("8062002"),
        Some(&(0, None)),
        "#8062 : une piste jamais jouée rend 0 et null, pas une clé absente"
    );
    assert_eq!(
        p1.get("8062003"),
        Some(&(0, None)),
        "#8062 : le profil 1 ne voit pas les écoutes du profil 2"
    );
    assert_eq!(
        p1.get("8062004"),
        Some(&(1, Some("2026-11-03T12:00:00Z".to_string()))),
        "#8062 : une écoute sans profil compte pour tous"
    );

    let p2 = repo
        .ecoutes_de_service_par_identifiant("qobuz", &ids, 2)
        .unwrap();
    assert_eq!(
        p2.get("8062001"),
        Some(&(0, None)),
        "#8062 : le profil 2 ne doit pas voir les écoutes du profil 1"
    );
    assert_eq!(p2.get("8062003").map(|e| e.0), Some(1));
    assert_eq!(p2.get("8062004").map(|e| e.0), Some(1));

    assert!(
        repo.ecoutes_de_service_par_identifiant("qobuz", &[], 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn sqlite_ecoutes_des_pistes_de_service_par_profil_8062() {
    let base = SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    let repo = jeu_d_ecoutes(&db);
    verifier(&repo);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8062_ecoutes_des_pistes_de_service_par_profil() {
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
    db.execute(
        "DELETE FROM listen_history WHERE source_id LIKE '8062%'",
        &[],
    )
    .unwrap();
    let repo = jeu_d_ecoutes(&db);
    verifier(&repo);
    db.execute(
        "DELETE FROM listen_history WHERE source_id LIKE '8062%'",
        &[],
    )
    .unwrap();
}
