//! [rc14] `poser_code_barres_si_vide` : remplit une colonne vide, n'écrase
//! jamais une valeur connue. Même requête en SQLite et en PostgreSQL.
use super::album_repo::AlbumRepo;
use super::backend::DbBackend;
use std::sync::Arc;

fn verifier(db: Arc<dyn DbBackend>) {
    db.execute(
        "CREATE TEMP TABLE albums (id BIGINT PRIMARY KEY, barcode TEXT)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO albums (id, barcode) VALUES (1, NULL), (2, ''), (3, '5099902987620')",
        &[],
    )
    .unwrap();
    let repo = AlbumRepo::with_backend(db.clone());
    assert!(repo.poser_code_barres_si_vide(1, "0028947758419").unwrap());
    assert!(repo.poser_code_barres_si_vide(2, "602547924865").unwrap());
    assert!(!repo.poser_code_barres_si_vide(3, "0028947758419").unwrap());
    // Une seconde pose sur une colonne désormais remplie ne change rien.
    assert!(!repo.poser_code_barres_si_vide(1, "40085423").unwrap());
    let lu = |id: i64| -> Option<String> {
        db.query_one("SELECT barcode FROM albums WHERE id = ?", &[&id])
            .unwrap()
            .and_then(|c| c.first().and_then(|v| v.as_string()))
    };
    assert_eq!(lu(1).as_deref(), Some("0028947758419"));
    assert_eq!(lu(2).as_deref(), Some("602547924865"));
    assert_eq!(lu(3).as_deref(), Some("5099902987620"));
}

#[test]
fn code_barres_si_vide_sqlite() {
    verifier(Arc::new(super::sqlite::SqliteDb::open_in_memory().unwrap()));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_code_barres_si_vide() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // Une connexion : la table temporaire reste isolée des autres témoins.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    verifier(Arc::new(super::backend::PostgresBackend::new(pool.clone())));
    pool.close().await;
}
