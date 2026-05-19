use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Option<i64>,
    pub username: String,
    pub display_name: Option<String>,
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
    db: SqliteDb,
}

impl ProfileRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Profile>, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT id, username, display_name, avatar_path, is_admin, created_at FROM profiles WHERE id = ?",
            params![id],
            |row| Ok(row_to_profile(row)),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<Profile>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, username, display_name, avatar_path, is_admin, created_at FROM profiles ORDER BY id")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| Ok(row_to_profile(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn create(&self, username: &str, display_name: Option<&str>) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO profiles (username, display_name) VALUES (?, ?)",
            &[&username as &dyn rusqlite::types::ToSql, &display_name],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn update(&self, id: i64, display_name: Option<&str>, avatar_path: Option<&str>) -> Result<(), String> {
        self.db.execute(
            "UPDATE profiles SET display_name = COALESCE(?, display_name), avatar_path = COALESCE(?, avatar_path) WHERE id = ?",
            &[&display_name as &dyn rusqlite::types::ToSql, &avatar_path, &id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        if id == 1 {
            return Err("cannot delete default profile".into());
        }
        self.db.execute("DELETE FROM profiles WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn add_favorite(&self, profile_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        self.db.execute(
            "INSERT OR IGNORE INTO favorites (profile_id, item_type, item_id) VALUES (?, ?, ?)",
            &[&profile_id as &dyn rusqlite::types::ToSql, &item_type, &item_id],
        )?;
        Ok(())
    }

    pub fn remove_favorite(&self, profile_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        self.db.execute(
            "DELETE FROM favorites WHERE profile_id = ? AND item_type = ? AND item_id = ?",
            &[&profile_id as &dyn rusqlite::types::ToSql, &item_type, &item_id],
        )?;
        Ok(())
    }

    pub fn is_favorite(&self, profile_id: i64, item_type: &str, item_id: i64) -> Result<bool, String> {
        let conn = self.db.connection().lock().unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM favorites WHERE profile_id = ? AND item_type = ? AND item_id = ?",
                params![profile_id, item_type, item_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(count > 0)
    }

    pub fn list_favorites(&self, profile_id: i64, item_type: Option<&str>) -> Result<Vec<Favorite>, String> {
        let conn = self.db.connection().lock().unwrap();
        let (sql, param_values): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(t) = item_type {
            (
                "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = ? AND item_type = ? ORDER BY created_at DESC".into(),
                vec![Box::new(profile_id), Box::new(t.to_string())],
            )
        } else {
            (
                "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = ? ORDER BY created_at DESC".into(),
                vec![Box::new(profile_id)],
            )
        };
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let params: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let items = stmt
            .query_map(params.as_slice(), |row| {
                Ok(Favorite {
                    id: row.get(0).ok(),
                    profile_id: row.get(1).unwrap_or(1),
                    item_type: row.get(2).unwrap_or_default(),
                    item_id: row.get(3).unwrap_or(0),
                    created_at: row.get(4).ok().flatten(),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }
}

fn row_to_profile(row: &rusqlite::Row) -> Profile {
    Profile {
        id: row.get(0).ok(),
        username: row.get(1).unwrap_or_default(),
        display_name: row.get(2).ok().flatten(),
        avatar_path: row.get(3).ok().flatten(),
        is_admin: row.get::<_, i32>(4).unwrap_or(0) != 0,
        created_at: row.get(5).ok().flatten(),
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
        assert_eq!(profiles[0].username, "default");

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
}
