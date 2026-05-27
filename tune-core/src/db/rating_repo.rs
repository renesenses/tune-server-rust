use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

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
    db: SqliteDb,
}

impl RatingRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn rate_album(&self, album_id: i64, profile_id: i64, rating: i32, note: Option<&str>) -> Result<(), String> {
        if !(1..=5).contains(&rating) {
            return Err("rating must be 1-5".into());
        }
        self.db.execute(
            "INSERT INTO album_ratings (album_id, profile_id, rating, note) VALUES (?, ?, ?, ?) ON CONFLICT(album_id, profile_id) DO UPDATE SET rating = excluded.rating, note = excluded.note",
            &[&album_id as &dyn rusqlite::types::ToSql, &profile_id, &rating, &note],
        )?;
        Ok(())
    }

    pub fn get_rating(&self, album_id: i64, profile_id: i64) -> Result<Option<AlbumRating>, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT id, album_id, profile_id, rating, note, created_at FROM album_ratings WHERE album_id = ? AND profile_id = ?",
            params![album_id, profile_id],
            |row| Ok(AlbumRating {
                id: row.get(0).ok(),
                album_id: row.get(1).unwrap_or(0),
                profile_id: row.get(2).unwrap_or(1),
                rating: row.get(3).unwrap_or(0),
                note: row.get(4).ok().flatten(),
                created_at: row.get(5).ok().flatten(),
            }),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn top_rated(&self, limit: i64) -> Result<Vec<(i64, f64, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT album_id, AVG(rating) as avg_rating, COUNT(*) as count FROM album_ratings GROUP BY album_id ORDER BY avg_rating DESC LIMIT ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0),
                    row.get::<_, f64>(1).unwrap_or(0.0),
                    row.get::<_, i64>(2).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn delete_rating(&self, album_id: i64, profile_id: i64) -> Result<(), String> {
        self.db.execute(
            "DELETE FROM album_ratings WHERE album_id = ? AND profile_id = ?",
            &[&album_id as &dyn rusqlite::types::ToSql, &profile_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::models::Album;
    use crate::db::migrations;

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

        // Rating 1 should work
        repo.rate_album(aid, 1, 1, None).unwrap();
        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 1);

        // Rating 5 should work
        repo.rate_album(aid, 1, 5, None).unwrap();
        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 5);

        // Rating 0 should fail
        assert!(repo.rate_album(aid, 1, 0, None).is_err());
        // Rating 6 should fail
        assert!(repo.rate_album(aid, 1, 6, None).is_err());
        // Negative rating should fail
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
        repo.rate_album(aid, 1, 5, Some("Hard bop chef-d'oeuvre")).unwrap();

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
        assert_eq!(top[0].0, a3); // highest first
        assert_eq!(top[1].0, a2);
        assert_eq!(top[2].0, a1);
    }

    #[test]
    fn rating_multiple_profiles() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let album_repo = AlbumRepo::new(db.clone());
        let aid = album_repo.create(&Album::new("Test".into())).unwrap();

        // Create second profile
        let profile_repo = crate::db::profile_repo::ProfileRepo::new(db.clone());
        let pid2 = profile_repo.create("user2", None).unwrap();

        let repo = RatingRepo::new(db);
        repo.rate_album(aid, 1, 5, None).unwrap();
        repo.rate_album(aid, pid2, 3, None).unwrap();

        assert_eq!(repo.get_rating(aid, 1).unwrap().unwrap().rating, 5);
        assert_eq!(repo.get_rating(aid, pid2).unwrap().unwrap().rating, 3);

        // top_rated should average
        let top = repo.top_rated(10).unwrap();
        assert_eq!(top.len(), 1);
        assert!((top[0].1 - 4.0).abs() < 0.01); // avg of 5 and 3
        assert_eq!(top[0].2, 2); // 2 ratings
    }
}
