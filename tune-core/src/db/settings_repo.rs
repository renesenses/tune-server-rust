use rusqlite::{params, OptionalExtension};

use super::sqlite::SqliteDb;

pub struct SettingsRepo {
    db: SqliteDb,
}

impl SettingsRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.db.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            &[&key as &dyn rusqlite::types::ToSql, &value],
        )?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        self.db.execute("DELETE FROM settings WHERE key = ?", &[&key])?;
        Ok(())
    }

    pub fn all(&self) -> Result<Vec<(String, String)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, value FROM settings ORDER BY key")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
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
        assert_eq!(repo.get("music_dirs").unwrap().unwrap(), r#"["/music","/nas"]"#);

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 1);

        repo.delete("music_dirs").unwrap();
        assert!(repo.get("music_dirs").unwrap().is_none());
    }
}
