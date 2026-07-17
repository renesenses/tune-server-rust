use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// A favorited streaming item (Tidal/Qobuz/…). Unlike local `favorites` (keyed
/// on an INTEGER `item_id`), streaming items use string `service_id`s, so they
/// live in their own `streaming_favorites` table. Display metadata is stored so
/// the favorites list needs no per-item hydration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingFavorite {
    pub id: i64,
    pub profile_id: i64,
    pub item_type: String,
    pub service: String,
    pub service_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub created_at: Option<String>,
}

/// Engine-agnostic SQL builders.
pub mod sql {
    use super::SqlDialect;

    pub fn add<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO streaming_favorites \
             (profile_id, item_type, service, service_id, title, artist, album, cover_url) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, service, service_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
        )
    }

    pub fn remove<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_favorites \
             WHERE profile_id = {} AND item_type = {} AND service = {} AND service_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    pub fn count_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM streaming_favorites \
             WHERE profile_id = {} AND item_type = {} AND service = {} AND service_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    const SELECT_COLS: &str =
        "SELECT id, profile_id, item_type, service, service_id, title, artist, album, cover_url, created_at \
         FROM streaming_favorites";

    pub fn list_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS} WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_by_type<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS} WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }
}

pub struct StreamingFavoritesRepo {
    db: Arc<dyn DbBackend>,
}

impl StreamingFavoritesRepo {
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

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        cover_url: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::add, sql::add);
        let params: [&dyn ToSqlValue; 8] = [
            &profile_id,
            &item_type,
            &service,
            &service_id,
            &title,
            &artist,
            &album,
            &cover_url,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn remove(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::remove, sql::remove);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &item_type, &service, &service_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn is_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
    ) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_one, sql::count_one);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &item_type, &service, &service_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n > 0)
    }

    pub fn list(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
    ) -> Result<Vec<StreamingFavorite>, String> {
        let rows = if let Some(t) = item_type {
            let sql = self.dialect_sql(sql::list_by_type, sql::list_by_type);
            let params: [&dyn ToSqlValue; 2] = [&profile_id, &t];
            self.db.query_many(&sql, &params)?
        } else {
            let sql = self.dialect_sql(sql::list_all, sql::list_all);
            let params: [&dyn ToSqlValue; 1] = [&profile_id];
            self.db.query_many(&sql, &params)?
        };
        Ok(rows.iter().map(row_to_streaming_favorite).collect())
    }
}

fn row_to_streaming_favorite(cols: &Vec<SqlValue>) -> StreamingFavorite {
    StreamingFavorite {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        profile_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(1),
        item_type: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        service: cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
        service_id: cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
        title: cols.get(5).and_then(|v| v.as_string()),
        artist: cols.get(6).and_then(|v| v.as_string()),
        album: cols.get(7).and_then(|v| v.as_string()),
        cover_url: cols.get(8).and_then(|v| v.as_string()),
        created_at: cols.get(9).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_repo() -> StreamingFavoritesRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        StreamingFavoritesRepo::new(db)
    }

    #[test]
    fn add_list_remove() {
        let repo = fresh_repo();
        repo.add(1, "track", "tidal", "t1", Some("Song"), Some("Artist"), Some("Album"), None)
            .unwrap();
        repo.add(1, "album", "qobuz", "q9", Some("Rec"), Some("Band"), None, None)
            .unwrap();

        let all = repo.list(1, None).unwrap();
        assert_eq!(all.len(), 2);
        let tracks = repo.list(1, Some("track")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].service, "tidal");
        assert_eq!(tracks[0].service_id, "t1");
        assert_eq!(tracks[0].title.as_deref(), Some("Song"));

        assert!(repo.is_favorite(1, "track", "tidal", "t1").unwrap());
        assert!(!repo.is_favorite(1, "track", "tidal", "nope").unwrap());

        repo.remove(1, "track", "tidal", "t1").unwrap();
        assert!(!repo.is_favorite(1, "track", "tidal", "t1").unwrap());
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
    }

    #[test]
    fn add_is_idempotent_and_profile_scoped() {
        let repo = fresh_repo();
        repo.add(1, "track", "tidal", "t1", None, None, None, None).unwrap();
        repo.add(1, "track", "tidal", "t1", None, None, None, None).unwrap();
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
        // Different profile is isolated.
        repo.add(2, "track", "tidal", "t1", None, None, None, None).unwrap();
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
        assert_eq!(repo.list(2, None).unwrap().len(), 1);
    }
}
