//! Metadata suggestions store.
//!
//! Goes through the selected [`DbBackend`], not a raw `SqliteDb`: in
//! PostgreSQL mode the server no longer opens SQLite at all, so a store bound
//! to a private SQLite handle would have been writing to a file nothing else
//! reads (the split-brain the audit flagged).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::Engine;

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

/// Columns every read below selects, in this order. [`row_to_suggestion`]
/// depends on it.
const COLUMNS: &str = "id, track_id, album_id, field, suggested_value, source, confidence, status";

fn row_to_suggestion(row: &[SqlValue]) -> Option<MetadataSuggestion> {
    Some(MetadataSuggestion {
        id: row.first()?.as_i64()?,
        track_id: row.get(1).and_then(|v| v.as_i64()),
        album_id: row.get(2).and_then(|v| v.as_i64()),
        field: row.get(3)?.as_string()?,
        suggested_value: row.get(4)?.as_string()?,
        source: row.get(5)?.as_string()?,
        confidence: row.get(6).and_then(|v| v.as_f64()).unwrap_or(0.0),
        status: row.get(7)?.as_string()?,
    })
}

pub struct SuggestionStore {
    backend: Arc<dyn DbBackend>,
}

impl SuggestionStore {
    pub fn with_backend(backend: Arc<dyn DbBackend>) -> Self {
        Self { backend }
    }

    /// Create the table if it does not exist.
    ///
    /// The DDL is dialect-specific: `INTEGER PRIMARY KEY AUTOINCREMENT` is
    /// SQLite-only and is a syntax error on PostgreSQL, which is one reason
    /// this store could never have worked against a PG backend before.
    pub fn setup_table(&self) -> Result<(), String> {
        let id_column = match self.backend.engine() {
            Engine::Postgres => "id BIGSERIAL PRIMARY KEY",
            Engine::Sqlite => "id INTEGER PRIMARY KEY AUTOINCREMENT",
        };
        self.backend.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS metadata_suggestions (
                {id_column},
                track_id BIGINT,
                album_id BIGINT,
                field TEXT NOT NULL,
                suggested_value TEXT NOT NULL,
                source TEXT NOT NULL,
                confidence DOUBLE PRECISION NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_ms_track ON metadata_suggestions(track_id);
            CREATE INDEX IF NOT EXISTS idx_ms_album ON metadata_suggestions(album_id);
            CREATE INDEX IF NOT EXISTS idx_ms_status ON metadata_suggestions(status);"
        ))
    }

    pub fn add_track_suggestion(
        &self,
        track_id: i64,
        field: &str,
        value: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64, String> {
        self.add_suggestion("track_id", track_id, field, value, source, confidence)
    }

    pub fn add_album_suggestion(
        &self,
        album_id: i64,
        field: &str,
        value: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64, String> {
        self.add_suggestion("album_id", album_id, field, value, source, confidence)
    }

    /// `owner_column` is a hard-coded `"track_id"` / `"album_id"` from the two
    /// callers above — never caller input, so interpolating it is safe.
    fn add_suggestion(
        &self,
        owner_column: &str,
        owner_id: i64,
        field: &str,
        value: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64, String> {
        let field = field.to_string();
        let value = value.to_string();
        let source = source.to_string();
        self.backend.execute_returning_id(
            &format!(
                "INSERT INTO metadata_suggestions \
                 ({owner_column}, field, suggested_value, source, confidence, status) \
                 VALUES (?, ?, ?, ?, ?, 'pending')"
            ),
            &[
                &owner_id as &dyn ToSqlValue,
                &field as &dyn ToSqlValue,
                &value as &dyn ToSqlValue,
                &source as &dyn ToSqlValue,
                &confidence as &dyn ToSqlValue,
            ],
        )
    }

    pub fn pending_for_track(&self, track_id: i64) -> Result<Vec<MetadataSuggestion>, String> {
        self.query_suggestions(
            &format!(
                "SELECT {COLUMNS} FROM metadata_suggestions \
                 WHERE track_id = ? AND status = 'pending' ORDER BY confidence DESC"
            ),
            track_id,
        )
    }

    pub fn pending_for_album(&self, album_id: i64) -> Result<Vec<MetadataSuggestion>, String> {
        self.query_suggestions(
            &format!(
                "SELECT {COLUMNS} FROM metadata_suggestions \
                 WHERE album_id = ? AND status = 'pending' ORDER BY confidence DESC"
            ),
            album_id,
        )
    }

    pub fn accept(&self, suggestion_id: i64) -> Result<Option<MetadataSuggestion>, String> {
        self.set_status(suggestion_id, "accepted")?;
        let row = self.backend.query_one(
            &format!("SELECT {COLUMNS} FROM metadata_suggestions WHERE id = ?"),
            &[&suggestion_id as &dyn ToSqlValue],
        )?;
        Ok(row.as_deref().and_then(row_to_suggestion))
    }

    pub fn reject(&self, suggestion_id: i64) -> Result<(), String> {
        self.set_status(suggestion_id, "rejected")
    }

    fn set_status(&self, suggestion_id: i64, status: &str) -> Result<(), String> {
        let status = status.to_string();
        self.backend
            .execute(
                "UPDATE metadata_suggestions SET status = ? WHERE id = ?",
                &[
                    &status as &dyn ToSqlValue,
                    &suggestion_id as &dyn ToSqlValue,
                ],
            )
            .map(|_| ())
    }

    pub fn auto_apply_above(&self, threshold: f64) -> Result<Vec<MetadataSuggestion>, String> {
        let rows = self.backend.query_many(
            &format!(
                "SELECT {COLUMNS} FROM metadata_suggestions \
                 WHERE status = 'pending' AND confidence >= ?"
            ),
            &[&threshold as &dyn ToSqlValue],
        )?;
        let suggestions: Vec<MetadataSuggestion> =
            rows.iter().filter_map(|r| row_to_suggestion(r)).collect();

        for s in &suggestions {
            self.set_status(s.id, "accepted")?;
        }
        Ok(suggestions)
    }

    pub fn count_pending(&self) -> Result<i64, String> {
        Ok(self
            .backend
            .query_one(
                "SELECT COUNT(*) FROM metadata_suggestions WHERE status = 'pending'",
                &[],
            )?
            .and_then(|row| row.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }

    pub fn clear(&self) -> Result<(), String> {
        self.backend
            .execute("DELETE FROM metadata_suggestions", &[])
            .map(|_| ())
    }

    fn query_suggestions(&self, sql: &str, param: i64) -> Result<Vec<MetadataSuggestion>, String> {
        let rows = self.backend.query_many(sql, &[&param as &dyn ToSqlValue])?;
        Ok(rows.iter().filter_map(|r| row_to_suggestion(r)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> SuggestionStore {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let store = SuggestionStore::with_backend(backend);
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
