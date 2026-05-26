use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
}

pub struct TagRepo {
    db: SqliteDb,
}

impl TagRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn list(&self) -> Result<Vec<Tag>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, color FROM tags ORDER BY name")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| {
                Ok(Tag {
                    id: row.get(0).ok(),
                    name: row.get(1).unwrap_or_default(),
                    color: row.get(2).unwrap_or_else(|_| "#808080".into()),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn create(&self, name: &str, color: Option<&str>) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO tags (name, color) VALUES (?, ?)",
            &[&name as &dyn rusqlite::types::ToSql, &color.unwrap_or("#808080")],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Tag>, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT id, name, color FROM tags WHERE id = ?",
            params![id],
            |row| {
                Ok(Tag {
                    id: row.get(0).ok(),
                    name: row.get(1).unwrap_or_default(),
                    color: row.get(2).unwrap_or_else(|_| "#808080".into()),
                })
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn update(&self, id: i64, name: Option<&str>, color: Option<&str>) -> Result<(), String> {
        if let Some(name) = name {
            self.db.execute(
                "UPDATE tags SET name = ? WHERE id = ?",
                &[&name as &dyn rusqlite::types::ToSql, &id],
            )?;
        }
        if let Some(color) = color {
            self.db.execute(
                "UPDATE tags SET color = ? WHERE id = ?",
                &[&color as &dyn rusqlite::types::ToSql, &id],
            )?;
        }
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM tags WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn all_items_by_tag(&self, tag_id: i64) -> Result<Vec<(String, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT item_type, item_id FROM item_tags WHERE tag_id = ? ORDER BY item_type, item_id")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![tag_id], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, i64>(1).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn tag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        self.db.execute(
            "INSERT OR IGNORE INTO item_tags (tag_id, item_type, item_id) VALUES (?, ?, ?)",
            &[&tag_id as &dyn rusqlite::types::ToSql, &item_type, &item_id],
        )?;
        Ok(())
    }

    pub fn untag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        self.db.execute(
            "DELETE FROM item_tags WHERE tag_id = ? AND item_type = ? AND item_id = ?",
            &[&tag_id as &dyn rusqlite::types::ToSql, &item_type, &item_id],
        )?;
        Ok(())
    }

    pub fn items_by_tag(&self, tag_id: i64, item_type: &str) -> Result<Vec<i64>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT item_id FROM item_tags WHERE tag_id = ? AND item_type = ? ORDER BY item_id")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map(params![tag_id, item_type], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    pub fn tags_for_item(&self, item_type: &str, item_id: i64) -> Result<Vec<Tag>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT t.id, t.name, t.color FROM tags t JOIN item_tags it ON t.id = it.tag_id WHERE it.item_type = ? AND it.item_id = ? ORDER BY t.name")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![item_type, item_id], |row| {
                Ok(Tag {
                    id: row.get(0).ok(),
                    name: row.get(1).unwrap_or_default(),
                    color: row.get(2).unwrap_or_else(|_| "#808080".into()),
                })
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
    fn tags_crud_and_tagging() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);

        let id = repo.create("Jazz", Some("#FFD700")).unwrap();
        let tags = repo.list().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].color, "#FFD700");

        repo.tag_item(id, "album", 1).unwrap();
        repo.tag_item(id, "album", 2).unwrap();

        let items = repo.items_by_tag(id, "album").unwrap();
        assert_eq!(items, vec![1, 2]);

        let album_tags = repo.tags_for_item("album", 1).unwrap();
        assert_eq!(album_tags.len(), 1);

        repo.untag_item(id, "album", 1).unwrap();
        assert_eq!(repo.items_by_tag(id, "album").unwrap(), vec![2]);

        repo.delete(id).unwrap();
        assert!(repo.list().unwrap().is_empty());
    }
}
