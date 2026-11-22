//! #9260 — le rafraîchissement COMPLET des clés de tri, lancé en fond.
//!
//! Sous PostgreSQL, le fil `cles-de-tri` était un fil std nu : le moteur y
//! paniquait (« there is no reactor running ») et `complet_en_cours` restait
//! pris pour toute la vie du processus — les clés n'étaient plus jamais
//! rafraîchies. Témoins : le rafraîchissement aboutit (la clé est écrite ET
//! le drapeau relâché) sur PostgreSQL et sur SQLite, et une panique du travail
//! relâche le drapeau.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::db::backend::DbBackend;
use crate::db::models::Track;

/// Une piste dont la clé de tri est absente.
fn piste_sans_cle(db: &Arc<dyn DbBackend>, chemin: &str) -> i64 {
    let mut t = Track::new("Titre 9260".into());
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.source = "local".into();
    t.sample_rate = Some(44_100);
    t.channels = 2;
    let id = TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    db.execute("UPDATE tracks SET cle_tri = NULL WHERE id = ?", &[&id])
        .unwrap();
    id
}

fn cle(db: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
    db.query_one("SELECT cle_tri FROM tracks WHERE id = ?", &[&id])
        .unwrap()
        .and_then(|l| l.first().and_then(|v| v.as_str().map(str::to_string)))
}

fn en_cours() -> bool {
    ETAT_DES_CLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .complet_en_cours
}

/// Lance le rafraîchissement complet (urgent) et attend qu'il ait écrit la
/// clé de `id` et relâché le drapeau. D'autres témoins du même binaire
/// partagent l'état : tant qu'aucun rafraîchissement ne tourne et que la clé
/// manque, on relance ; un drapeau resté pris fait échouer à l'échéance.
fn rafraichir_et_attendre(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = TrackRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        let pris = en_cours();
        let ecrite = cle(db, id).is_some();
        if ecrite && !pris {
            return;
        }
        assert!(
            debut.elapsed() < Duration::from_secs(30),
            "#9260 : rafraîchissement des clés de tri jamais abouti \
             (clé écrite : {ecrite}, complet_en_cours resté pris : {pris})"
        );
        if !pris && !ecrite {
            repo.rafraichir_en_fond(None, true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SQLite, sans exécuteur Tokio (le repli du fil à lui) : non-régression.
#[test]
fn sqlite_le_rafraichissement_de_fond_aboutit_9260() {
    let base = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    crate::db::migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    let id = piste_sans_cle(&db, "/m/9260/a.flac");
    assert_eq!(cle(&db, id), None);
    rafraichir_et_attendre(&db, id);
}

/// SQLite, dans un exécuteur Tokio (le pool bloquant) : non-régression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_le_rafraichissement_de_fond_aboutit_sous_tokio_9260() {
    tokio::task::spawn_blocking(|| {
        let base = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        base.init_schema().unwrap();
        crate::db::migrations::run_migrations(&base).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(base);
        let id = piste_sans_cle(&db, "/m/9260/b.flac");
        rafraichir_et_attendre(&db, id);
    })
    .await
    .unwrap();
}

/// Une panique du travail relâche `complet_en_cours` (garde `Drop`).
#[test]
fn une_panique_relache_complet_en_cours_9260() {
    let resultat = std::panic::catch_unwind(|| {
        let mut fin = FinDuRafraichissement { parti: false };
        ETAT_DES_CLES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complet_en_cours = true;
        fin.parti = true;
        let _fin = fin;
        panic!("panique voulue (#9260)");
    });
    assert!(resultat.is_err());
    assert!(
        !en_cours(),
        "#9260 : complet_en_cours resté pris après une panique"
    );
}

/// PostgreSQL : le défaut lui-même. Sans le correctif, le fil nu panique à la
/// première requête et le drapeau ne retombe jamais.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_9260_le_rafraichissement_des_cles_de_tri_aboutit() {
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
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
    tokio::task::spawn_blocking(move || {
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();
        let id = piste_sans_cle(&db, "/m/9260/pg.flac");
        assert_eq!(cle(&db, id), None);
        rafraichir_et_attendre(&db, id);
    })
    .await
    .unwrap();
}
