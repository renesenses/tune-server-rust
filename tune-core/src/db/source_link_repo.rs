use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for source_link_repo.
pub mod sql {
    use super::SqlDialect;

    /// UPSERT on (track_id, service). Portable form (SQLite 3.24+, PG
    /// 9.5+). The `linked_at` value is now supplied as the 6th parameter
    /// so the SQL stays engine-agnostic (no more SQLite-only `strftime`).
    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO track_source_links (track_id, service, service_track_id, confidence, match_method, linked_at) \
             VALUES ({}, {}, {}, {}, {}, {}) \
             ON CONFLICT(track_id, service) DO UPDATE SET \
                service_track_id = excluded.service_track_id, \
                confidence = excluded.confidence, \
                match_method = excluded.match_method, \
                linked_at = excluded.linked_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
        )
    }

    pub fn get_by_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, track_id, service, service_track_id, confidence, match_method, linked_at \
             FROM track_source_links WHERE track_id = {}",
            d.placeholder(1)
        )
    }

    pub fn count_by_service<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM track_source_links WHERE service = {}",
            d.placeholder(1)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLink {
    pub id: Option<i64>,
    pub track_id: i64,
    pub service: String,
    pub service_track_id: String,
    pub confidence: f64,
    pub match_method: Option<String>,
    pub linked_at: Option<String>,
}

pub struct SourceLinkRepo {
    db: Arc<dyn DbBackend>,
}

impl SourceLinkRepo {
    /// Backward-compatible constructor — wraps `SqliteDb` in
    /// `Arc<dyn DbBackend>` to fit the trait-object storage. Every
    /// existing call site continues to pass a `SqliteDb` unchanged.
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    /// New constructor for callers that already hold a backend handle
    /// (Postgres or SQLite).
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

    pub fn upsert(
        &self,
        track_id: i64,
        service: &str,
        service_track_id: &str,
        confidence: f64,
        method: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let params: [&dyn ToSqlValue; 6] = [
            &track_id,
            &service,
            &service_track_id,
            &confidence,
            &method,
            &now,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_by_track(&self, track_id: i64) -> Result<Vec<SourceLink>, String> {
        let sql = self.dialect_sql(sql::get_by_track, sql::get_by_track);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        let rows = self.db.query_many(&sql, &params)?;
        rows.iter().map(row_to_link).collect()
    }

    pub fn count_by_service(&self, service: &str) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::count_by_service, sql::count_by_service);
        let params: [&dyn ToSqlValue; 1] = [&service];
        match self.db.query_one(&sql, &params)? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }
}

fn row_to_link(cols: &Vec<super::backend::SqlValue>) -> Result<SourceLink, String> {
    if cols.len() < 7 {
        return Err(format!(
            "source_link row: expected 7 cols, got {}",
            cols.len()
        ));
    }
    Ok(SourceLink {
        id: cols[0].as_i64(),
        track_id: cols[1].as_i64().ok_or("track_id null")?,
        service: cols[2].as_string().ok_or("service null")?,
        service_track_id: cols[3].as_string().ok_or("service_track_id null")?,
        confidence: cols[4].as_f64().ok_or("confidence null")?,
        match_method: cols[5].as_string(),
        linked_at: cols[6].as_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn upsert_and_get() {
        let db = test_db();
        let repo = SourceLinkRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());

        let track = crate::db::models::Track::new("Test".into());
        let track_id = track_repo.create(&track).unwrap();

        repo.upsert(track_id, "tidal", "tidal:123", 0.95, "exact")
            .unwrap();
        let links = repo.get_by_track(track_id).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].service, "tidal");
        assert_eq!(links[0].confidence, 0.95);
    }

    #[test]
    fn upsert_replaces() {
        let db = test_db();
        let repo = SourceLinkRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());

        let track = crate::db::models::Track::new("Test".into());
        let track_id = track_repo.create(&track).unwrap();

        repo.upsert(track_id, "tidal", "tidal:123", 0.8, "fuzzy")
            .unwrap();
        repo.upsert(track_id, "tidal", "tidal:456", 0.95, "exact")
            .unwrap();
        let links = repo.get_by_track(track_id).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].service_track_id, "tidal:456");
        assert_eq!(links[0].confidence, 0.95);
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::upsert(&s).contains("VALUES (?, ?, ?, ?, ?, ?)"));
        assert!(sql::upsert(&p).contains("VALUES ($1, $2, $3, $4, $5, $6)"));
        assert!(sql::get_by_track(&p).ends_with("WHERE track_id = $1"));
    }

    #[test]
    fn multiple_services() {
        let db = test_db();
        let repo = SourceLinkRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());

        let track = crate::db::models::Track::new("Test".into());
        let track_id = track_repo.create(&track).unwrap();

        repo.upsert(track_id, "tidal", "t:1", 0.9, "exact").unwrap();
        repo.upsert(track_id, "qobuz", "q:1", 0.85, "fuzzy")
            .unwrap();
        let links = repo.get_by_track(track_id).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(repo.count_by_service("tidal").unwrap(), 1);
        assert_eq!(repo.count_by_service("qobuz").unwrap(), 1);
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let track = crate::db::models::Track::new("Test".into());
        let track_id = track_repo.create(&track).unwrap();

        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = SourceLinkRepo::with_backend(backend);
        repo.upsert(track_id, "tidal", "t:1", 0.9, "exact").unwrap();
        assert_eq!(repo.count_by_service("tidal").unwrap(), 1);
    }
}
