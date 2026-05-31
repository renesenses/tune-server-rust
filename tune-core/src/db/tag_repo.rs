use rusqlite::{OptionalExtension, params};
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
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn create(&self, name: &str, color: Option<&str>) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO tags (name, color) VALUES (?, ?)",
            &[
                &name as &dyn rusqlite::types::ToSql,
                &color.unwrap_or("#808080"),
            ],
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
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
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
            .prepare(
                "SELECT item_id FROM item_tags WHERE tag_id = ? AND item_type = ? ORDER BY item_id",
            )
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map(params![tag_id, item_type], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
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

    #[test]
    fn tag_update() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let id = repo.create("Rock", Some("#FF0000")).unwrap();

        repo.update(id, Some("Rock & Roll"), Some("#00FF00"))
            .unwrap();
        let tag = repo.get(id).unwrap().unwrap();
        assert_eq!(tag.name, "Rock & Roll");
        assert_eq!(tag.color, "#00FF00");
    }

    #[test]
    fn tag_default_color() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let id = repo.create("NoColor", None).unwrap();
        let tag = repo.get(id).unwrap().unwrap();
        assert_eq!(tag.color, "#808080");
    }

    #[test]
    fn tag_get_nonexistent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn tag_all_items_by_tag() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Favorites", None).unwrap();

        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 2).unwrap();
        repo.tag_item(tag_id, "track", 10).unwrap();
        repo.tag_item(tag_id, "artist", 5).unwrap();

        let all_items = repo.all_items_by_tag(tag_id).unwrap();
        assert_eq!(all_items.len(), 4);
        // Should be sorted by item_type, item_id
        assert_eq!(all_items[0].0, "album");
        assert_eq!(all_items[0].1, 1);
    }

    #[test]
    fn tag_item_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Test", None).unwrap();

        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 1).unwrap(); // Should not error

        let items = repo.items_by_tag(tag_id, "album").unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn tag_multiple_tags_per_item() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let jazz = repo.create("Jazz", Some("#FFD700")).unwrap();
        let fav = repo.create("Favorites", Some("#FF0000")).unwrap();

        repo.tag_item(jazz, "album", 1).unwrap();
        repo.tag_item(fav, "album", 1).unwrap();

        let album_tags = repo.tags_for_item("album", 1).unwrap();
        assert_eq!(album_tags.len(), 2);
    }

    #[test]
    fn tag_delete_cascades_items() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("ToDelete", None).unwrap();
        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 2).unwrap();

        repo.delete(tag_id).unwrap();
        // After deleting the tag, item_tags should be cascade-deleted
        assert!(repo.get(tag_id).unwrap().is_none());
    }

    #[test]
    fn tag_list_sorted() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        repo.create("Zebra", None).unwrap();
        repo.create("Alpha", None).unwrap();
        repo.create("Middle", None).unwrap();

        let tags = repo.list().unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0].name, "Alpha");
        assert_eq!(tags[2].name, "Zebra");
    }
}
