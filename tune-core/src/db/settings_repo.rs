use rusqlite::{OptionalExtension, params};

use super::engine::SqlDialect;
use super::sqlite::SqliteDb;

pub struct SettingsRepo {
    db: SqliteDb,
}

/// Engine-agnostic SQL builders. They live as free functions so the
/// future PostgresRepo can call them with `PostgresDialect` while the
/// SQLite repo below uses `SqliteDialect`.
pub mod sql {
    use super::SqlDialect;

    pub fn get_by_key<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT value FROM settings WHERE key = {}",
            d.placeholder(1)
        )
    }

    pub fn delete_by_key<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM settings WHERE key = {}", d.placeholder(1))
    }

    pub fn list_all() -> &'static str {
        "SELECT key, value FROM settings ORDER BY key"
    }
}

impl SettingsRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row(&sql::get_by_key(&self.db.dialect()), params![key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        // The UPSERT form below is supported identically by SQLite (3.24+)
        // and PostgreSQL (9.5+), so we keep a single literal here.
        self.db.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            &[&key as &dyn rusqlite::types::ToSql, &value],
        )?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        self.db
            .execute(&sql::delete_by_key(&self.db.dialect()), &[&key])?;
        Ok(())
    }

    pub fn all(&self) -> Result<Vec<(String, String)>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn.prepare(sql::list_all()).map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn settings_crud() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);

        assert!(repo.get("music_dirs").unwrap().is_none());

        repo.set("music_dirs", r#"["/music"]"#).unwrap();
        assert_eq!(repo.get("music_dirs").unwrap().unwrap(), r#"["/music"]"#);

        repo.set("music_dirs", r#"["/music","/nas"]"#).unwrap();
        assert_eq!(
            repo.get("music_dirs").unwrap().unwrap(),
            r#"["/music","/nas"]"#
        );

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 1);

        repo.delete("music_dirs").unwrap();
        assert!(repo.get("music_dirs").unwrap().is_none());
    }

    #[test]
    fn settings_multiple_keys() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        repo.set("key1", "value1").unwrap();
        repo.set("key2", "value2").unwrap();
        repo.set("key3", "value3").unwrap();

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 3);
        // All() should be sorted by key
        assert_eq!(all[0].0, "key1");
        assert_eq!(all[1].0, "key2");
        assert_eq!(all[2].0, "key3");
    }

    #[test]
    fn settings_overwrite() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        repo.set("theme", "dark").unwrap();
        repo.set("theme", "light").unwrap();
        assert_eq!(repo.get("theme").unwrap().unwrap(), "light");
        assert_eq!(repo.all().unwrap().len(), 1);
    }

    #[test]
    fn settings_delete_nonexistent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        // Should not error
        repo.delete("nonexistent").unwrap();
    }

    #[test]
    fn settings_empty_value() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        repo.set("empty", "").unwrap();
        assert_eq!(repo.get("empty").unwrap().unwrap(), "");
    }

    #[test]
    fn settings_json_value() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        let json = r#"{"enabled":true,"services":["tidal","qobuz"]}"#;
        repo.set("streaming_config", json).unwrap();
        assert_eq!(repo.get("streaming_config").unwrap().unwrap(), json);
    }

    #[test]
    fn settings_unicode_key_and_value() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = SettingsRepo::new(db);
        repo.set("nom_utilisateur", "Rene").unwrap();
        assert_eq!(repo.get("nom_utilisateur").unwrap().unwrap(), "Rene");
    }

    #[test]
    fn sql_builders_emit_sqlite_placeholders() {
        use crate::db::engine::SqliteDialect;
        let d = SqliteDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = ?"
        );
        assert_eq!(sql::delete_by_key(&d), "DELETE FROM settings WHERE key = ?");
        assert_eq!(
            sql::list_all(),
            "SELECT key, value FROM settings ORDER BY key"
        );
    }

    #[test]
    fn sql_builders_emit_postgres_placeholders() {
        use crate::db::engine::PostgresDialect;
        let d = PostgresDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = $1"
        );
        assert_eq!(
            sql::delete_by_key(&d),
            "DELETE FROM settings WHERE key = $1"
        );
    }
}
