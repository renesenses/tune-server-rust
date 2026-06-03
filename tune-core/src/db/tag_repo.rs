use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for tag_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn list_all() -> &'static str {
        "SELECT id, name, color FROM tags ORDER BY name"
    }

    pub fn create_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO tags (name, color) VALUES ({}, {})",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, name, color FROM tags WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn update_name<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tags SET name = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_color<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tags SET color = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_by_id<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tags WHERE id = {}", d.placeholder(1))
    }

    pub fn all_items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_type, item_id FROM item_tags WHERE tag_id = {} ORDER BY item_type, item_id",
            d.placeholder(1)
        )
    }

    /// INSERT OR IGNORE rewritten to portable ON CONFLICT DO NOTHING.
    /// UNIQUE(tag_id, item_type, item_id) is enforced by the schema.
    pub fn tag_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES ({}, {}, {}) ON CONFLICT (tag_id, item_type, item_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn untag_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM item_tags WHERE tag_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_id FROM item_tags WHERE tag_id = {} AND item_type = {} ORDER BY item_id",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn tags_for_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT t.id, t.name, t.color FROM tags t JOIN item_tags it ON t.id = it.tag_id WHERE it.item_type = {} AND it.item_id = {} ORDER BY t.name",
            d.placeholder(1),
            d.placeholder(2)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
}

pub struct TagRepo {
    db: Arc<dyn DbBackend>,
}

impl TagRepo {
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

    pub fn list(&self) -> Result<Vec<Tag>, String> {
        let rows = self.db.query_many(sql::list_all(), &[])?;
        Ok(rows.iter().map(row_to_tag).collect())
    }

    pub fn create(&self, name: &str, color: Option<&str>) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create_tag, sql::create_tag);
        let color_val = color.unwrap_or("#808080");
        let params: [&dyn ToSqlValue; 2] = [&name, &color_val];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Tag>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_tag))
    }

    pub fn update(&self, id: i64, name: Option<&str>, color: Option<&str>) -> Result<(), String> {
        if let Some(name) = name {
            let sql = self.dialect_sql(sql::update_name, sql::update_name);
            let params: [&dyn ToSqlValue; 2] = [&name, &id];
            self.db.execute(&sql, &params)?;
        }
        if let Some(color) = color {
            let sql = self.dialect_sql(sql::update_color, sql::update_color);
            let params: [&dyn ToSqlValue; 2] = [&color, &id];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_id, sql::delete_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn all_items_by_tag(&self, tag_id: i64) -> Result<Vec<(String, i64)>, String> {
        let sql = self.dialect_sql(sql::all_items_by_tag, sql::all_items_by_tag);
        let params: [&dyn ToSqlValue; 1] = [&tag_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn tag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::tag_item, sql::tag_item);
        let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn untag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::untag_item, sql::untag_item);
        let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn items_by_tag(&self, tag_id: i64, item_type: &str) -> Result<Vec<i64>, String> {
        let sql = self.dialect_sql(sql::items_by_tag, sql::items_by_tag);
        let params: [&dyn ToSqlValue; 2] = [&tag_id, &item_type];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn tags_for_item(&self, item_type: &str, item_id: i64) -> Result<Vec<Tag>, String> {
        let sql = self.dialect_sql(sql::tags_for_item, sql::tags_for_item);
        let params: [&dyn ToSqlValue; 2] = [&item_type, &item_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_tag).collect())
    }
}

fn row_to_tag(cols: &Vec<SqlValue>) -> Tag {
    Tag {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        color: cols
            .get(2)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "#808080".into()),
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
        repo.tag_item(tag_id, "album", 1).unwrap();

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
        assert!(repo.get(tag_id).unwrap().is_none());
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create_tag(&s).contains("VALUES (?, ?)"));
        assert!(sql::create_tag(&p).contains("VALUES ($1, $2)"));
        assert!(sql::tag_item(&p).ends_with("ON CONFLICT (tag_id, item_type, item_id) DO NOTHING"));
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

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = TagRepo::with_backend(backend);
        let id = repo.create("X", None).unwrap();
        assert!(repo.get(id).unwrap().is_some());
    }
}
