//! #8672 (décision de Bertrand du 13/11/2026) — « Ma collection physique » :
//! les disques associés À LA MAIN à un album de la bibliothèque.
//!
//! Une ligne par release Discogs (`collection_physique_associations`,
//! migration SQLite 153 / PostgreSQL 119) : release → album local. Le greffon
//! `tune-collection-physique` la lit à chaque affichage ; le lien y prime sur
//! le rapprochement automatique. Rangé dans la base et non dans la mémoire du
//! greffon (`collection.json`) : les synchronisations Discogs et « Effacer le
//! cache » ne le touchent pas.
//!
//! Sans clé étrangère : un album qui quitte la bibliothèque laisse la ligne,
//! que le greffon ignore tant que l'album n'est pas revenu.

use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};

/// Les associations rangées : release Discogs → album local.
pub fn lire(db: &Arc<dyn DbBackend>) -> Result<HashMap<u64, i64>, String> {
    let lignes = db.query_many(
        "SELECT release_id, album_id FROM collection_physique_associations",
        &[],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| {
            let release = l.first()?.as_i64()?;
            let album = l.get(1)?.as_i64()?;
            Some((u64::try_from(release).ok()?, album))
        })
        .collect())
}

/// Associe la release à l'album ; remplace une association précédente.
/// `maintenant` : secondes depuis l'époque Unix.
pub fn associer(
    db: &Arc<dyn DbBackend>,
    release: u64,
    album: i64,
    maintenant: i64,
) -> Result<(), String> {
    let release = i64::try_from(release).map_err(|_| "release_hors_bornes".to_string())?;
    db.execute(
        "INSERT INTO collection_physique_associations (release_id, album_id, associe_le) \
         VALUES (?, ?, ?) \
         ON CONFLICT(release_id) DO UPDATE SET album_id = excluded.album_id, \
         associe_le = excluded.associe_le",
        &[&release as &dyn ToSqlValue, &album, &maintenant],
    )?;
    Ok(())
}

/// Défait l'association de la release. `true` si elle existait.
pub fn dissocier(db: &Arc<dyn DbBackend>, release: u64) -> Result<bool, String> {
    let Ok(release) = i64::try_from(release) else {
        return Ok(false);
    };
    let n = db.execute(
        "DELETE FROM collection_physique_associations WHERE release_id = ?",
        &[&release as &dyn ToSqlValue],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Releases propres à ces essais : la base PostgreSQL d'essai est
    /// partagée, on n'y touche à rien d'autre.
    const R1: u64 = 8_672_000_001;
    const R2: u64 = 8_672_000_002;

    fn verifier(db: Arc<dyn DbBackend>) {
        let _ = dissocier(&db, R1);
        let _ = dissocier(&db, R2);
        let des_miennes = |db: &Arc<dyn DbBackend>| -> Vec<(u64, i64)> {
            let mut v: Vec<(u64, i64)> = lire(db)
                .unwrap()
                .into_iter()
                .filter(|(r, _)| *r == R1 || *r == R2)
                .collect();
            v.sort();
            v
        };
        assert!(des_miennes(&db).is_empty());

        associer(&db, R1, 7, 1_000).unwrap();
        associer(&db, R2, 9, 1_000).unwrap();
        assert_eq!(des_miennes(&db), vec![(R1, 7), (R2, 9)]);

        // Associer de nouveau remplace (`ON CONFLICT … DO UPDATE`).
        associer(&db, R1, 8, 2_000).unwrap();
        assert_eq!(des_miennes(&db), vec![(R1, 8), (R2, 9)]);

        assert!(dissocier(&db, R1).unwrap(), "elle existait");
        assert!(!dissocier(&db, R1).unwrap(), "plus rien à défaire");
        assert_eq!(des_miennes(&db), vec![(R2, 9)]);
        assert!(dissocier(&db, R2).unwrap());
        assert!(des_miennes(&db).is_empty());
    }

    #[test]
    fn associations_manuelles_sqlite_8672() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        verifier(Arc::new(db));
    }

    /// Sur un vrai PostgreSQL : la PG 119 pose la table, `?` devient `$n`,
    /// l'`ON CONFLICT … DO UPDATE` et les BIGINT.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_8672_associations_manuelles() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
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
        let db: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        verifier(db);
        pool.close().await;
    }
}
