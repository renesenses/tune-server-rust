use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// A user-submitted report about wrong metadata (bad cover, wrong artist
/// image, bogus credit…). Local first: the row is the record of truth, and the
/// community push is best-effort on top of it.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataReport {
    pub id: i64,
    pub entity: String,
    pub entity_id: Option<i64>,
    pub mbid: Option<String>,
    pub field: Option<String>,
    pub value: Option<String>,
    pub reason: String,
    pub comment: Option<String>,
    pub created_at: String,
    pub pushed_at: Option<String>,
}

/// Entities a report can target. Kept in sync with the cloud whitelist.
pub const REPORT_ENTITIES: &[&str] = &[
    "track",
    "album",
    "artist",
    "cover",
    "artist_image",
    "bio",
    "extra",
];

/// Engine-agnostic SQL builders for metadata_report_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn insert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO metadata_reports \
             (entity, entity_id, mbid, field, value, reason, comment, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8)
        )
    }

    pub const SELECT_COLS: &str = "id, entity, entity_id, mbid, field, value, reason, comment, \
                                   created_at, pushed_at";

    pub fn list<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {SELECT_COLS} FROM metadata_reports ORDER BY id DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn list_unpushed<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {SELECT_COLS} FROM metadata_reports WHERE pushed_at IS NULL \
             ORDER BY id LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn mark_pushed<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE metadata_reports SET pushed_at = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM metadata_reports WHERE id = {}",
            d.placeholder(1)
        )
    }
}

pub struct MetadataReportRepo {
    db: Arc<dyn DbBackend>,
}

impl MetadataReportRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    /// Record a report. `created_at` is stamped by the caller-provided clock so
    /// tests stay deterministic.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        entity: &str,
        entity_id: Option<i64>,
        mbid: Option<&str>,
        field: Option<&str>,
        value: Option<&str>,
        reason: &str,
        comment: Option<&str>,
        created_at: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::insert, sql::insert);
        let params: [&dyn ToSqlValue; 8] = [
            &entity,
            &entity_id,
            &mbid,
            &field,
            &value,
            &reason,
            &comment,
            &created_at,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    fn rows_to_reports(rows: Vec<Vec<super::backend::SqlValue>>) -> Vec<MetadataReport> {
        rows.into_iter()
            .filter_map(|cols| {
                Some(MetadataReport {
                    id: cols.first().and_then(|v| v.as_i64())?,
                    entity: cols.get(1).and_then(|v| v.as_string())?,
                    entity_id: cols.get(2).and_then(|v| v.as_i64()),
                    mbid: cols.get(3).and_then(|v| v.as_string()),
                    field: cols.get(4).and_then(|v| v.as_string()),
                    value: cols.get(5).and_then(|v| v.as_string()),
                    reason: cols.get(6).and_then(|v| v.as_string())?,
                    comment: cols.get(7).and_then(|v| v.as_string()),
                    created_at: cols.get(8).and_then(|v| v.as_string()).unwrap_or_default(),
                    pushed_at: cols.get(9).and_then(|v| v.as_string()),
                })
            })
            .collect()
    }

    /// Most recent reports first.
    pub fn list(&self, limit: i64) -> Result<Vec<MetadataReport>, String> {
        let sql = self.dialect_sql(sql::list, sql::list);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        Ok(Self::rows_to_reports(self.db.query_many(&sql, &params)?))
    }

    /// Reports not yet accepted by the community backend, oldest first.
    pub fn list_unpushed(&self, limit: i64) -> Result<Vec<MetadataReport>, String> {
        let sql = self.dialect_sql(sql::list_unpushed, sql::list_unpushed);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        Ok(Self::rows_to_reports(self.db.query_many(&sql, &params)?))
    }

    pub fn mark_pushed(&self, id: i64, pushed_at: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::mark_pushed, sql::mark_pushed);
        let params: [&dyn ToSqlValue; 2] = [&pushed_at, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn setup_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn insert_and_list() {
        let repo = MetadataReportRepo::new(setup_db());
        repo.insert(
            "artist_image",
            Some(42),
            None,
            None,
            None,
            "incorrect_image",
            None,
            "2026-08-06T10:00:00Z",
        )
        .unwrap();

        let reports = repo.list(10).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].entity, "artist_image");
        assert_eq!(reports[0].entity_id, Some(42));
        assert_eq!(reports[0].reason, "incorrect_image");
        assert!(reports[0].pushed_at.is_none());
    }

    #[test]
    fn list_is_newest_first() {
        let repo = MetadataReportRepo::new(setup_db());
        for (i, reason) in ["first", "second", "third"].iter().enumerate() {
            repo.insert(
                "album",
                Some(i as i64),
                None,
                None,
                None,
                reason,
                None,
                "2026-08-06T10:00:00Z",
            )
            .unwrap();
        }
        let reports = repo.list(10).unwrap();
        assert_eq!(reports[0].reason, "third");
        assert_eq!(reports[2].reason, "first");
    }

    #[test]
    fn unpushed_excludes_pushed_and_is_oldest_first() {
        let repo = MetadataReportRepo::new(setup_db());
        repo.insert(
            "extra",
            None,
            Some("mbid-1"),
            Some("conductor"),
            Some("Wrong Name"),
            "wrong_value",
            None,
            "2026-08-06T10:00:00Z",
        )
        .unwrap();
        repo.insert(
            "cover",
            Some(7),
            None,
            None,
            None,
            "wrong_cover",
            Some("ce n'est pas cet album"),
            "2026-08-06T10:01:00Z",
        )
        .unwrap();

        let pending = repo.list_unpushed(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].entity, "extra");
        assert_eq!(pending[0].field.as_deref(), Some("conductor"));
        assert_eq!(
            pending[1].comment.as_deref(),
            Some("ce n'est pas cet album")
        );

        repo.mark_pushed(pending[0].id, "2026-08-06T10:05:00Z")
            .unwrap();
        let still_pending = repo.list_unpushed(10).unwrap();
        assert_eq!(still_pending.len(), 1);
        assert_eq!(still_pending[0].entity, "cover");

        let all = repo.list(10).unwrap();
        let pushed = all.iter().find(|r| r.entity == "extra").unwrap();
        assert_eq!(pushed.pushed_at.as_deref(), Some("2026-08-06T10:05:00Z"));
    }

    #[test]
    fn delete_removes_row() {
        let repo = MetadataReportRepo::new(setup_db());
        repo.insert(
            "bio",
            Some(3),
            None,
            None,
            None,
            "wrong_bio",
            None,
            "2026-08-06T10:00:00Z",
        )
        .unwrap();
        let id = repo.list(10).unwrap()[0].id;
        repo.delete(id).unwrap();
        assert!(repo.list(10).unwrap().is_empty());
    }

    #[test]
    fn entities_whitelist_covers_reportable_surfaces() {
        for e in [
            "track",
            "album",
            "artist",
            "cover",
            "artist_image",
            "bio",
            "extra",
        ] {
            assert!(REPORT_ENTITIES.contains(&e), "{e} missing from whitelist");
        }
    }
}
