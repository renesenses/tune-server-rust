use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for album_metadata_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn get_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT key, value FROM album_metadata WHERE album_id = {} ORDER BY key",
            d.placeholder(1)
        )
    }

    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO album_metadata (album_id, key, value) VALUES ({}, {}, {}) \
             ON CONFLICT (album_id, key) DO UPDATE SET value = excluded.value",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_metadata WHERE album_id = {} AND key = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_metadata WHERE album_id = {}",
            d.placeholder(1)
        )
    }
}

/// Album-level extended metadata (Vademecum k/v: conductor, performer,
/// barcode, catalog_number…), symmetric with [`super::track_metadata_repo`].
/// Before this store existed the web UI parked album-scoped fields on the
/// album's FIRST track, so they vanished when that track was rescanned.
pub struct AlbumMetadataRepo {
    db: Arc<dyn DbBackend>,
}

impl AlbumMetadataRepo {
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

    /// Get all metadata key-value pairs for an album.
    pub fn get_all(&self, album_id: i64) -> Result<HashMap<String, String>, String> {
        let sql = self.dialect_sql(sql::get_all, sql::get_all);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
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
    pub fn set(&self, album_id: i64, key: &str, value: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 3] = [&album_id, &key, &value];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Set multiple metadata fields in a batch (upsert each).
    pub fn set_batch(&self, album_id: i64, fields: &HashMap<String, String>) -> Result<(), String> {
        if fields.is_empty() {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        for (key, value) in fields {
            let params: [&dyn ToSqlValue; 3] = [&album_id, &key.as_str(), &value.as_str()];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    /// Delete a single metadata field.
    pub fn delete(&self, album_id: i64, key: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_one, sql::delete_one);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &key];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Delete all metadata for an album.
    pub fn delete_all(&self, album_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_all, sql::delete_all);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
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
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Test Artist');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Test Album', 1);",
        )
        .unwrap();
        db
    }

    #[test]
    fn set_and_get() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "barcode", "0028947758419").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("conductor").unwrap(), "Karajan");
        assert_eq!(meta.get("barcode").unwrap(), "0028947758419");
    }

    #[test]
    fn upsert_overwrites() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "conductor", "Abbado").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.get("conductor").unwrap(), "Abbado");
    }

    #[test]
    fn set_batch() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        let mut fields = HashMap::new();
        fields.insert("performer".into(), "Berliner Philharmoniker".into());
        fields.insert("catalog_number".into(), "477 5842".into());

        repo.set_batch(1, &fields).unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("catalog_number").unwrap(), "477 5842");
    }

    #[test]
    fn delete_one_and_all() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "performer", "BPO").unwrap();
        repo.delete(1, "conductor").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 1);
        assert!(meta.get("conductor").is_none());

        repo.delete_all(1).unwrap();
        assert!(repo.get_all(1).unwrap().is_empty());
    }

    #[test]
    fn empty_album_returns_empty_map() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        let meta = repo.get_all(1).unwrap();
        assert!(meta.is_empty());
    }
}
