use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for track_metadata_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn get_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT key, value FROM track_metadata WHERE track_id = {} ORDER BY key",
            d.placeholder(1)
        )
    }

    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO track_metadata (track_id, key, value) VALUES ({}, {}, {}) \
             ON CONFLICT (track_id, key) DO UPDATE SET value = excluded.value",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM track_metadata WHERE track_id = {} AND key = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM track_metadata WHERE track_id = {}",
            d.placeholder(1)
        )
    }

    /// Search track_metadata for rows where `key` is one of the searchable
    /// metadata fields and `value` matches a LIKE pattern.
    /// Returns (track_id, key, value) triples.
    pub fn search_by_value<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT DISTINCT track_id, key, value FROM track_metadata \
             WHERE key IN ('composer','conductor','lyricist','performer','remixer',\
             'producer','label','comment','lyrics','isrc','catalog_number') \
             AND LOWER(value) LIKE LOWER({}) \
             LIMIT {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }
}

pub struct TrackMetadataRepo {
    db: Arc<dyn DbBackend>,
}

impl TrackMetadataRepo {
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

    /// Get all metadata key-value pairs for a track.
    pub fn get_all(&self, track_id: i64) -> Result<HashMap<String, String>, String> {
        let sql = self.dialect_sql(sql::get_all, sql::get_all);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        let rows = self.db.query_many(&sql, &params)?;
        let mut map = HashMap::new();
        for cols in rows {
            let key = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
            let value = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
        Ok(map)
    }

    /// Set a single metadata field (upsert).
    pub fn set(&self, track_id: i64, key: &str, value: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 3] = [&track_id, &key, &value];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Set multiple metadata fields in a batch (upsert each).
    pub fn set_batch(&self, track_id: i64, fields: &HashMap<String, String>) -> Result<(), String> {
        if fields.is_empty() {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        for (key, value) in fields {
            let params: [&dyn ToSqlValue; 3] = [&track_id, &key.as_str(), &value.as_str()];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    /// Delete a single metadata field.
    pub fn delete(&self, track_id: i64, key: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_one, sql::delete_one);
        let params: [&dyn ToSqlValue; 2] = [&track_id, &key];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Delete all metadata for a track.
    pub fn delete_all(&self, track_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_all, sql::delete_all);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Search track_metadata for tracks whose searchable metadata fields
    /// contain the given query string. Returns a vec of (track_id, key, value)
    /// for each matching row.
    pub fn search_by_value(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(i64, String, String)>, String> {
        let like = format!("%{query}%");
        let sql = self.dialect_sql(sql::search_by_value, sql::search_by_value);
        let params: [&dyn ToSqlValue; 2] = [&like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| {
                let track_id = cols.first().and_then(|v| v.as_i64())?;
                let key = cols.get(1).and_then(|v| v.as_string())?;
                let value = cols.get(2).and_then(|v| v.as_string())?;
                Some((track_id, key, value))
            })
            .collect())
    }

    /// Batch-set metadata for multiple tracks at once (used by scanner).
    /// Takes a vec of (track_id, fields) pairs and inserts them all.
    pub fn set_batch_multi(
        &self,
        entries: &[(i64, HashMap<String, String>)],
    ) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        // TEMP DEBUG: log the generated SQL and engine once
        tracing::warn!(
            engine = ?self.db.engine(),
            sql = %sql,
            entry_count = entries.len(),
            "set_batch_multi_debug_start"
        );
        let mut total_rows = 0usize;
        let mut first_logged = false;
        for (track_id, fields) in entries {
            if fields.is_empty() {
                tracing::warn!(track_id, "set_batch_multi_debug_empty_fields");
                continue;
            }
            for (key, value) in fields {
                let params: [&dyn ToSqlValue; 3] = [track_id, &key.as_str(), &value.as_str()];
                match self.db.execute(&sql, &params) {
                    Ok(rows_affected) => {
                        total_rows += rows_affected;
                        if !first_logged {
                            tracing::warn!(
                                track_id,
                                key = %key,
                                value_len = value.len(),
                                rows_affected,
                                "set_batch_multi_debug_first_insert_ok"
                            );
                            first_logged = true;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            track_id,
                            key = %key,
                            error = %e,
                            "set_batch_multi_debug_insert_failed"
                        );
                        return Err(e);
                    }
                }
            }
        }
        tracing::warn!(total_rows, first_logged, "set_batch_multi_debug_complete");
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
        // Insert a dummy track so FK constraint is satisfied
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Test Artist');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Test Album', 1);
             INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
             VALUES (1, 'Test Track', 1, 1, 300000, 1, 1, 2);",
        )
        .unwrap();
        db
    }

    #[test]
    fn set_and_get() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "composer", "Bach").unwrap();
        repo.set(1, "conductor", "Karajan").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("composer").unwrap(), "Bach");
        assert_eq!(meta.get("conductor").unwrap(), "Karajan");
    }

    #[test]
    fn upsert_overwrites() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "composer", "Bach").unwrap();
        repo.set(1, "composer", "Mozart").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.get("composer").unwrap(), "Mozart");
    }

    #[test]
    fn set_batch() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        let mut fields = HashMap::new();
        fields.insert("composer".into(), "Beethoven".into());
        fields.insert("conductor".into(), "Furtwangler".into());
        fields.insert("label".into(), "DG".into());

        repo.set_batch(1, &fields).unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 3);
        assert_eq!(meta.get("label").unwrap(), "DG");
    }

    #[test]
    fn delete_one() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "composer", "Bach").unwrap();
        repo.set(1, "conductor", "Karajan").unwrap();
        repo.delete(1, "composer").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 1);
        assert!(meta.get("composer").is_none());
        assert_eq!(meta.get("conductor").unwrap(), "Karajan");
    }

    #[test]
    fn delete_all() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "composer", "Bach").unwrap();
        repo.set(1, "conductor", "Karajan").unwrap();
        repo.delete_all(1).unwrap();

        let meta = repo.get_all(1).unwrap();
        assert!(meta.is_empty());
    }

    #[test]
    fn empty_track_returns_empty_map() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        let meta = repo.get_all(1).unwrap();
        assert!(meta.is_empty());
    }

    #[test]
    fn with_backend_constructor() {
        let db = setup_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = TrackMetadataRepo::with_backend(backend);
        repo.set(1, "mood", "happy").unwrap();
        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.get("mood").unwrap(), "happy");
    }

    #[test]
    fn search_by_value_finds_conductor() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "conductor", "Herbert von Karajan").unwrap();
        repo.set(1, "bpm", "120").unwrap(); // non-searchable field

        let results = repo.search_by_value("Karajan", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[0].1, "conductor");
        assert_eq!(results[0].2, "Herbert von Karajan");
    }

    #[test]
    fn search_by_value_excludes_non_searchable_keys() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        // bpm is not in the searchable keys list
        repo.set(1, "bpm", "120").unwrap();
        repo.set(1, "rg_track_gain", "-3.5").unwrap();
        repo.set(1, "mb_recording_id", "abc-123").unwrap();

        let results = repo.search_by_value("120", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_by_value_case_insensitive() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "label", "Deutsche Grammophon").unwrap();

        let results = repo.search_by_value("deutsche", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_by_value_empty_when_no_match() {
        let db = setup_db();
        let repo = TrackMetadataRepo::new(db);

        repo.set(1, "composer", "Bach").unwrap();

        let results = repo.search_by_value("Mozart", 10).unwrap();
        assert!(results.is_empty());
    }
}
