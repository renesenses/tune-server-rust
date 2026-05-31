use crate::db::sqlite::SqliteDb;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSuggestion {
    pub id: i64,
    pub track_id: Option<i64>,
    pub album_id: Option<i64>,
    pub field: String,
    pub suggested_value: String,
    pub source: String,
    pub confidence: f64,
    pub status: String,
}

pub struct SuggestionStore {
    db: SqliteDb,
}

impl SuggestionStore {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata_suggestions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id INTEGER,
                album_id INTEGER,
                field TEXT NOT NULL,
                suggested_value TEXT NOT NULL,
                source TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_ms_track ON metadata_suggestions(track_id);
            CREATE INDEX IF NOT EXISTS idx_ms_album ON metadata_suggestions(album_id);
            CREATE INDEX IF NOT EXISTS idx_ms_status ON metadata_suggestions(status);",
        )
    }

    pub fn add_track_suggestion(
        &self,
        track_id: i64,
        field: &str,
        value: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metadata_suggestions \
             (track_id, field, suggested_value, source, confidence, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
            rusqlite::params![track_id, field, value, source, confidence],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn add_album_suggestion(
        &self,
        album_id: i64,
        field: &str,
        value: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metadata_suggestions \
             (album_id, field, suggested_value, source, confidence, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
            rusqlite::params![album_id, field, value, source, confidence],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn pending_for_track(&self, track_id: i64) -> Result<Vec<MetadataSuggestion>, String> {
        self.query_suggestions(
            "SELECT id, track_id, album_id, field, suggested_value, source, confidence, status \
             FROM metadata_suggestions WHERE track_id = ?1 AND status = 'pending' ORDER BY confidence DESC",
            track_id,
        )
    }

    pub fn pending_for_album(&self, album_id: i64) -> Result<Vec<MetadataSuggestion>, String> {
        self.query_suggestions(
            "SELECT id, track_id, album_id, field, suggested_value, source, confidence, status \
             FROM metadata_suggestions WHERE album_id = ?1 AND status = 'pending' ORDER BY confidence DESC",
            album_id,
        )
    }

    pub fn accept(&self, suggestion_id: i64) -> Result<Option<MetadataSuggestion>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE metadata_suggestions SET status = 'accepted' WHERE id = ?1",
            [suggestion_id],
        )
        .map_err(|e| e.to_string())?;

        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, album_id, field, suggested_value, source, confidence, status \
                 FROM metadata_suggestions WHERE id = ?1",
            )
            .map_err(|e| e.to_string())?;

        let result = stmt
            .query_row([suggestion_id], |row| {
                Ok(MetadataSuggestion {
                    id: row.get(0)?,
                    track_id: row.get(1)?,
                    album_id: row.get(2)?,
                    field: row.get(3)?,
                    suggested_value: row.get(4)?,
                    source: row.get(5)?,
                    confidence: row.get(6)?,
                    status: row.get(7)?,
                })
            })
            .ok();

        Ok(result)
    }

    pub fn reject(&self, suggestion_id: i64) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE metadata_suggestions SET status = 'rejected' WHERE id = ?1",
            [suggestion_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn auto_apply_above(&self, threshold: f64) -> Result<Vec<MetadataSuggestion>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, album_id, field, suggested_value, source, confidence, status \
                 FROM metadata_suggestions WHERE status = 'pending' AND confidence >= ?1",
            )
            .map_err(|e| e.to_string())?;

        let suggestions: Vec<MetadataSuggestion> = stmt
            .query_map([threshold], |row| {
                Ok(MetadataSuggestion {
                    id: row.get(0)?,
                    track_id: row.get(1)?,
                    album_id: row.get(2)?,
                    field: row.get(3)?,
                    suggested_value: row.get(4)?,
                    source: row.get(5)?,
                    confidence: row.get(6)?,
                    status: row.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        for s in &suggestions {
            conn.execute(
                "UPDATE metadata_suggestions SET status = 'accepted' WHERE id = ?1",
                [s.id],
            )
            .map_err(|e| e.to_string())?;
        }

        Ok(suggestions)
    }

    pub fn count_pending(&self) -> Result<i64, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM metadata_suggestions WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.db.execute_batch("DELETE FROM metadata_suggestions")
    }

    fn query_suggestions(&self, sql: &str, param: i64) -> Result<Vec<MetadataSuggestion>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([param], |row| {
                Ok(MetadataSuggestion {
                    id: row.get(0)?,
                    track_id: row.get(1)?,
                    album_id: row.get(2)?,
                    field: row.get(3)?,
                    suggested_value: row.get(4)?,
                    source: row.get(5)?,
                    confidence: row.get(6)?,
                    status: row.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> SuggestionStore {
        let db = SqliteDb::open_in_memory().unwrap();
        let store = SuggestionStore::new(db);
        store.setup_table().unwrap();
        store
    }

    #[test]
    fn add_and_query_track_suggestion() {
        let store = setup();
        let id = store
            .add_track_suggestion(1, "genre", "Jazz", "lastfm", 0.85)
            .unwrap();
        assert!(id > 0);

        let pending = store.pending_for_track(1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].field, "genre");
        assert_eq!(pending[0].suggested_value, "Jazz");
    }

    #[test]
    fn add_and_query_album_suggestion() {
        let store = setup();
        store
            .add_album_suggestion(10, "year", "1959", "musicbrainz", 0.95)
            .unwrap();

        let pending = store.pending_for_album(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].suggested_value, "1959");
    }

    #[test]
    fn accept_suggestion() {
        let store = setup();
        let id = store
            .add_track_suggestion(1, "isrc", "USRC123", "musicbrainz", 0.99)
            .unwrap();

        let accepted = store.accept(id).unwrap();
        assert!(accepted.is_some());
        assert_eq!(accepted.unwrap().status, "accepted");

        let pending = store.pending_for_track(1).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn reject_suggestion() {
        let store = setup();
        let id = store
            .add_track_suggestion(1, "label", "Wrong", "discogs", 0.5)
            .unwrap();

        store.reject(id).unwrap();
        let pending = store.pending_for_track(1).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn auto_apply_above_threshold() {
        let store = setup();
        store
            .add_track_suggestion(1, "genre", "Jazz", "lastfm", 0.7)
            .unwrap();
        store
            .add_track_suggestion(2, "year", "2020", "musicbrainz", 0.95)
            .unwrap();

        let applied = store.auto_apply_above(0.9).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].suggested_value, "2020");

        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn count_and_clear() {
        let store = setup();
        store.add_track_suggestion(1, "a", "v", "s", 0.5).unwrap();
        store.add_track_suggestion(2, "b", "w", "s", 0.5).unwrap();
        assert_eq!(store.count_pending().unwrap(), 2);

        store.clear().unwrap();
        assert_eq!(store.count_pending().unwrap(), 0);
    }
}
