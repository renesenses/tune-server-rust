use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

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
    db: SqliteDb,
}

impl SourceLinkRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn upsert(
        &self,
        track_id: i64,
        service: &str,
        service_track_id: &str,
        confidence: f64,
        method: &str,
    ) -> Result<(), String> {
        self.db.execute(
            "INSERT INTO track_source_links (track_id, service, service_track_id, confidence, match_method)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(track_id, service) DO UPDATE SET
                service_track_id = excluded.service_track_id,
                confidence = excluded.confidence,
                match_method = excluded.match_method,
                linked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            &[
                &track_id as &dyn rusqlite::types::ToSql,
                &service,
                &service_track_id,
                &confidence,
                &method,
            ],
        )?;
        Ok(())
    }

    pub fn get_by_track(&self, track_id: i64) -> Result<Vec<SourceLink>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, service, service_track_id, confidence, match_method, linked_at \
                 FROM track_source_links WHERE track_id = ?",
            )
            .map_err(|e| e.to_string())?;
        stmt.query_map(params![track_id], |row| {
            Ok(SourceLink {
                id: row.get(0).ok(),
                track_id: row.get(1)?,
                service: row.get(2)?,
                service_track_id: row.get(3)?,
                confidence: row.get(4)?,
                match_method: row.get(5).ok(),
                linked_at: row.get(6).ok(),
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
    }

    pub fn count_by_service(&self, service: &str) -> Result<i64, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM track_source_links WHERE service = ?",
            params![service],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
    }
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

        repo.upsert(track_id, "tidal", "tidal:123", 0.95, "exact").unwrap();
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

        repo.upsert(track_id, "tidal", "tidal:123", 0.8, "fuzzy").unwrap();
        repo.upsert(track_id, "tidal", "tidal:456", 0.95, "exact").unwrap();
        let links = repo.get_by_track(track_id).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].service_track_id, "tidal:456");
        assert_eq!(links[0].confidence, 0.95);
    }

    #[test]
    fn multiple_services() {
        let db = test_db();
        let repo = SourceLinkRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());

        let track = crate::db::models::Track::new("Test".into());
        let track_id = track_repo.create(&track).unwrap();

        repo.upsert(track_id, "tidal", "t:1", 0.9, "exact").unwrap();
        repo.upsert(track_id, "qobuz", "q:1", 0.85, "fuzzy").unwrap();
        let links = repo.get_by_track(track_id).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(repo.count_by_service("tidal").unwrap(), 1);
        assert_eq!(repo.count_by_service("qobuz").unwrap(), 1);
    }
}
