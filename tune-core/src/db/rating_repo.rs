use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for rating_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn upsert_rating<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO album_ratings (album_id, profile_id, rating, note) VALUES ({}, {}, {}, {}) ON CONFLICT(album_id, profile_id) DO UPDATE SET rating = excluded.rating, note = excluded.note",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn get_rating<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, album_id, profile_id, rating, note, created_at FROM album_ratings WHERE album_id = {} AND profile_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn top_rated<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT album_id, AVG(rating) as avg_rating, COUNT(*) as count FROM album_ratings GROUP BY album_id ORDER BY avg_rating DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn delete_rating<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_ratings WHERE album_id = {} AND profile_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumRating {
    pub id: Option<i64>,
    pub album_id: i64,
    pub profile_id: i64,
    pub rating: i32,
    pub note: Option<String>,
    pub created_at: Option<String>,
}

pub struct RatingRepo {
    db: Arc<dyn DbBackend>,
}

impl RatingRepo {
    /// Backward-compatible constructor — wraps `SqliteDb` in
    /// `Arc<dyn DbBackend>`. All existing call sites unchanged.
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

    pub fn rate_album(
        &self,
        album_id: i64,
        profile_id: i64,
        rating: i32,
        note: Option<&str>,
    ) -> Result<(), String> {
        if !(1..=5).contains(&rating) {
            return Err("rating must be 1-5".into());
        }
        let sql = self.dialect_sql(sql::upsert_rating, sql::upsert_rating);
        let params: [&dyn ToSqlValue; 4] = [&album_id, &profile_id, &rating, &note];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_rating(
        &self,
        album_id: i64,
        profile_id: i64,
    ) -> Result<Option<AlbumRating>, String> {
        let sql = self.dialect_sql(sql::get_rating, sql::get_rating);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &profile_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(None),
            Some(cols) => Ok(Some(row_to_rating(&cols)?)),
        }
    }

    pub fn top_rated(&self, limit: i64) -> Result<Vec<(i64, f64, i64)>, String> {
        let sql = self.dialect_sql(sql::top_rated, sql::top_rated);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
                    cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn delete_rating(&self, album_id: i64, profile_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_rating, sql::delete_rating);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &profile_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }
}

fn row_to_rating(cols: &[SqlValue]) -> Result<AlbumRating, String> {
    if cols.len() < 6 {
        return Err(format!("rating row: expected 6 cols, got {}", cols.len()));
    }
    Ok(AlbumRating {
        id: cols[0].as_i64(),
        album_id: cols[1].as_i64().unwrap_or(0),
        profile_id: cols[2].as_i64().unwrap_or(1),
        rating: cols[3].as_i64().unwrap_or(0) as i32,
        note: cols[4].as_string(),
        created_at: cols[5].as_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::migrations;
    use crate::db::models::Album;

    #[test]
    fn rate_and_query() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let a1 = Album::new("Kind of Blue".into());
        let id1 = album_repo.create(&a1).unwrap();
        let a2 = Album::new("A Love Supreme".into());
        let id2 = album_repo.create(&a2).unwrap();

        let repo = RatingRepo::new(db);

        assert!(repo.rate_album(id1, 1, 0, None).is_err());
        assert!(repo.rate_album(id1, 1, 6, None).is_err());

        repo.rate_album(id1, 1, 5, Some("Chef-d'oeuvre")).unwrap();
        repo.rate_album(id2, 1, 3, None).unwrap();

        let r = repo.get_rating(id1, 1).unwrap().unwrap();
        assert_eq!(r.rating, 5);
        assert_eq!(r.note.as_deref(), Some("Chef-d'oeuvre"));

        repo.rate_album(id1, 1, 4, None).unwrap();
        let r2 = repo.get_rating(id1, 1).unwrap().unwrap();
        assert_eq!(r2.rating, 4);

        let top = repo.top_rated(10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, id1);
    }

    #[test]
    fn delete_rating() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("Test".into())).unwrap();

        let repo = RatingRepo::new(db);
        repo.rate_album(aid, 1, 4, None).unwrap();
        assert!(repo.get_rating(aid, 1).unwrap().is_some());

        repo.delete_rating(aid, 1).unwrap();
        assert!(repo.get_rating(aid, 1).unwrap().is_none());
    }

    #[test]
    fn rating_boundary_values() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("Test".into())).unwrap();

        let repo = RatingRepo::new(db);

        repo.rate_album(aid, 1, 1, None).unwrap();
        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 1);

        repo.rate_album(aid, 1, 5, None).unwrap();
        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 5);

        assert!(repo.rate_album(aid, 1, 0, None).is_err());
        assert!(repo.rate_album(aid, 1, 6, None).is_err());
        assert!(repo.rate_album(aid, 1, -1, None).is_err());
    }

    #[test]
    fn rating_with_note() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("Blue Train".into())).unwrap();

        let repo = RatingRepo::new(db);
        repo.rate_album(aid, 1, 5, Some("Hard bop chef-d'oeuvre"))
            .unwrap();

        let r = repo.get_rating(aid, 1).unwrap().unwrap();
        assert_eq!(r.note.as_deref(), Some("Hard bop chef-d'oeuvre"));
    }

    #[test]
    fn rating_nonexistent_album() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RatingRepo::new(db);
        assert!(repo.get_rating(999, 1).unwrap().is_none());
    }

    #[test]
    fn top_rated_ordering() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let a1 = album_repo.create(&Album::new("Low rated".into())).unwrap();
        let a2 = album_repo.create(&Album::new("Mid rated".into())).unwrap();
        let a3 = album_repo.create(&Album::new("High rated".into())).unwrap();

        let repo = RatingRepo::new(db);
        repo.rate_album(a1, 1, 2, None).unwrap();
        repo.rate_album(a2, 1, 3, None).unwrap();
        repo.rate_album(a3, 1, 5, None).unwrap();

        let top = repo.top_rated(10).unwrap();
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].0, a3);
        assert_eq!(top[1].0, a2);
        assert_eq!(top[2].0, a1);
    }

    #[test]
    fn sql_builders_dialect_specific_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::upsert_rating(&s).contains("VALUES (?, ?, ?, ?)"));
        assert!(sql::upsert_rating(&p).contains("VALUES ($1, $2, $3, $4)"));
        assert!(sql::top_rated(&s).ends_with("LIMIT ?"));
        assert!(sql::top_rated(&p).ends_with("LIMIT $1"));
    }

    #[test]
    fn rating_multiple_profiles() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("Test".into())).unwrap();

        let profile_repo = crate::db::profile_repo::ProfileRepo::new(db.clone());
        let pid2 = profile_repo.create("user2", None).unwrap();

        let repo = RatingRepo::new(db);
        repo.rate_album(aid, 1, 5, None).unwrap();
        repo.rate_album(aid, pid2, 3, None).unwrap();

        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 5);
        assert_eq!(repo.get_rating(aid, pid2).unwrap().unwrap().rating, 3);

        let top = repo.top_rated(10).unwrap();
        assert_eq!(top.len(), 1);
        assert!((top[0].1 - 4.0).abs() < 0.01);
        assert_eq!(top[0].2, 2);
    }

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("X".into())).unwrap();

        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = RatingRepo::with_backend(backend);
        repo.rate_album(aid, 1, 4, None).unwrap();
        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 4);
    }
}
