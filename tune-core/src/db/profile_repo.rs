use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for profile_repo.
pub mod sql {
    use super::SqlDialect;

    const PROFILE_COLS: &str = "id, username, display_name, avatar_path, is_admin, created_at";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn list_all() -> &'static str {
        "SELECT id, username, display_name, avatar_path, is_admin, created_at FROM profiles ORDER BY id"
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO profiles (username, display_name) VALUES ({}, {})",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE profiles SET display_name = COALESCE({}, display_name), avatar_path = COALESCE({}, avatar_path) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM profiles WHERE id = {}", d.placeholder(1))
    }

    /// INSERT OR IGNORE form. Uses the portable ON CONFLICT DO NOTHING
    /// (SQLite 3.24+, PG 9.5+) so the same SQL runs on both engines.
    pub fn add_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES ({}, {}, {}) ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn remove_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM favorites WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn count_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM favorites WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn list_favorites_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_favorites_by_type<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Option<i64>,
    #[serde(alias = "username")]
    pub name: String,
    #[serde(alias = "display_name")]
    pub display_name: Option<String>,
    #[serde(alias = "avatar_path", rename = "avatar_color")]
    pub avatar_path: Option<String>,
    pub is_admin: bool,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub id: Option<i64>,
    pub profile_id: i64,
    pub item_type: String,
    pub item_id: i64,
    pub created_at: Option<String>,
}

pub struct ProfileRepo {
    db: Arc<dyn DbBackend>,
}

impl ProfileRepo {
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

    pub fn get(&self, id: i64) -> Result<Option<Profile>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_profile))
    }

    pub fn list(&self) -> Result<Vec<Profile>, String> {
        let rows = self.db.query_many(sql::list_all(), &[])?;
        Ok(rows.iter().map(row_to_profile).collect())
    }

    pub fn create(&self, username: &str, display_name: Option<&str>) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 2] = [&username, &display_name];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn update(
        &self,
        id: i64,
        display_name: Option<&str>,
        avatar_path: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 3] = [&display_name, &avatar_path, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        if id == 1 {
            return Err("cannot delete default profile".into());
        }
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn add_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::add_favorite, sql::add_favorite);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn remove_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::remove_favorite, sql::remove_favorite);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn is_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_favorite, sql::count_favorite);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &item_type, &item_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    pub fn list_favorites(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
    ) -> Result<Vec<Favorite>, String> {
        let rows = if let Some(t) = item_type {
            let sql = self.dialect_sql(sql::list_favorites_by_type, sql::list_favorites_by_type);
            let params: [&dyn ToSqlValue; 2] = [&profile_id, &t];
            self.db.query_many(&sql, &params)?
        } else {
            let sql = self.dialect_sql(sql::list_favorites_all, sql::list_favorites_all);
            let params: [&dyn ToSqlValue; 1] = [&profile_id];
            self.db.query_many(&sql, &params)?
        };
        Ok(rows.iter().map(row_to_favorite).collect())
    }
}

fn row_to_profile(cols: &Vec<SqlValue>) -> Profile {
    Profile {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        display_name: cols.get(2).and_then(|v| v.as_string()),
        avatar_path: cols.get(3).and_then(|v| v.as_string()),
        is_admin: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        created_at: cols.get(5).and_then(|v| v.as_string()),
    }
}

fn row_to_favorite(cols: &Vec<SqlValue>) -> Favorite {
    Favorite {
        id: cols.first().and_then(|v| v.as_i64()),
        profile_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(1),
        item_type: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        item_id: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        created_at: cols.get(4).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn profiles_and_favorites() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);

        let profiles = repo.list().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "default");

        let id = repo.create("bertrand", Some("Bertrand")).unwrap();
        assert!(id > 1);

        repo.add_favorite(1, "track", 42).unwrap();
        repo.add_favorite(1, "album", 10).unwrap();
        assert!(repo.is_favorite(1, "track", 42).unwrap());
        assert!(!repo.is_favorite(1, "track", 99).unwrap());

        let favs = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(favs.len(), 1);

        repo.remove_favorite(1, "track", 42).unwrap();
        assert!(!repo.is_favorite(1, "track", 42).unwrap());

        assert!(repo.delete(1).is_err());
        repo.delete(id).unwrap();
    }

    #[test]
    fn profile_update() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let id = repo.create("alice", Some("Alice")).unwrap();
        repo.update(id, Some("Alice Updated"), Some("/avatars/alice.png"))
            .unwrap();

        let p = repo.get(id).unwrap().unwrap();
        assert_eq!(p.display_name.as_deref(), Some("Alice Updated"));
        assert_eq!(p.avatar_path.as_deref(), Some("/avatars/alice.png"));
    }

    #[test]
    fn profile_get_default() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let default = repo.get(1).unwrap().unwrap();
        assert_eq!(default.name, "default");
        assert!(default.is_admin);
    }

    #[test]
    fn profile_favorites_multiple_types() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);

        repo.add_favorite(1, "track", 1).unwrap();
        repo.add_favorite(1, "track", 2).unwrap();
        repo.add_favorite(1, "album", 10).unwrap();
        repo.add_favorite(1, "artist", 5).unwrap();

        let all = repo.list_favorites(1, None).unwrap();
        assert_eq!(all.len(), 4);

        let tracks = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(tracks.len(), 2);

        let albums = repo.list_favorites(1, Some("album")).unwrap();
        assert_eq!(albums.len(), 1);
    }

    #[test]
    fn profile_duplicate_favorite_ignored() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        repo.add_favorite(1, "track", 42).unwrap();
        repo.add_favorite(1, "track", 42).unwrap();

        let favs = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(favs.len(), 1);
    }

    #[test]
    fn profile_get_nonexistent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn profile_multiple_users_separate_favorites() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let user2 = repo.create("bob", Some("Bob")).unwrap();

        repo.add_favorite(1, "track", 100).unwrap();
        repo.add_favorite(user2, "track", 200).unwrap();

        assert!(repo.is_favorite(1, "track", 100).unwrap());
        assert!(!repo.is_favorite(1, "track", 200).unwrap());
        assert!(!repo.is_favorite(user2, "track", 100).unwrap());
        assert!(repo.is_favorite(user2, "track", 200).unwrap());
    }

    #[test]
    fn profile_list_all() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        repo.create("alice", None).unwrap();
        repo.create("bob", None).unwrap();

        let all = repo.list().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn sql_builders_emit_dialect_specific_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;

        assert!(sql::get_by_id(&s).ends_with("WHERE id = ?"));
        assert!(sql::get_by_id(&p).ends_with("WHERE id = $1"));

        let pg_add = sql::add_favorite(&p);
        assert!(pg_add.contains("VALUES ($1, $2, $3)"));
        assert!(pg_add.ends_with("ON CONFLICT (profile_id, item_type, item_id) DO NOTHING"));

        let sqlite_add = sql::add_favorite(&s);
        assert!(sqlite_add.contains("VALUES (?, ?, ?)"));
        assert!(sqlite_add.ends_with("ON CONFLICT (profile_id, item_type, item_id) DO NOTHING"));
    }

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ProfileRepo::with_backend(backend);
        let id = repo.create("xx", None).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().name, "xx");
    }
}
