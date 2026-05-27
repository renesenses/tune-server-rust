use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenRecord {
    pub id: Option<i64>,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub source: String,
    pub duration_ms: i64,
    pub listened_at: Option<String>,
    pub zone_id: Option<i64>,
}

pub struct HistoryRepo {
    db: SqliteDb,
}

impl HistoryRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn record(&self, rec: &ListenRecord) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO listen_history (track_id, title, artist_name, album_title, source, duration_ms, zone_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                &rec.track_id as &dyn rusqlite::types::ToSql,
                &rec.title,
                &rec.artist_name,
                &rec.album_title,
                &rec.source,
                &rec.duration_ms,
                &rec.zone_id,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<ListenRecord>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, track_id, title, artist_name, album_title, source, duration_ms, listened_at, zone_id FROM listen_history ORDER BY listened_at DESC LIMIT ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| Ok(row_to_listen(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn top_tracks(&self, limit: i64) -> Result<Vec<(String, Option<String>, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT title, artist_name, COUNT(*) as plays FROM listen_history GROUP BY title, artist_name ORDER BY plays DESC LIMIT ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, Option<String>>(1).ok().flatten(),
                    row.get::<_, i64>(2).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn top_artists(&self, limit: i64) -> Result<Vec<(String, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT artist_name, COUNT(*) as plays FROM listen_history WHERE artist_name IS NOT NULL GROUP BY artist_name ORDER BY plays DESC LIMIT ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| {
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

    pub fn top_albums(&self, limit: i64) -> Result<Vec<(String, Option<String>, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT album_title, artist_name, COUNT(*) as plays FROM listen_history WHERE album_title IS NOT NULL GROUP BY album_title, artist_name ORDER BY plays DESC LIMIT ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, Option<String>>(1).ok().flatten(),
                    row.get::<_, i64>(2).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn listening_history(&self, days: i64) -> Result<Vec<(String, i64, i64)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT DATE(listened_at) as day, COUNT(*) as play_count, COALESCE(SUM(duration_ms), 0) as total_ms FROM listen_history WHERE DATE(listened_at) >= DATE('now', '-' || ? || ' days') GROUP BY day ORDER BY day")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![days], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, i64>(1).unwrap_or(0),
                    row.get::<_, i64>(2).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM listen_history", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn dashboard(&self) -> Result<DashboardStats, String> {
        let conn = self.db.connection().lock().unwrap();

        let total_listens: i64 = conn
            .query_row("SELECT COUNT(*) FROM listen_history", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;

        let total_duration_ms: i64 = conn
            .query_row("SELECT COALESCE(SUM(duration_ms), 0) FROM listen_history", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;

        let unique_tracks: i64 = conn
            .query_row("SELECT COUNT(DISTINCT title || COALESCE(artist_name, '')) FROM listen_history", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;

        let unique_artists: i64 = conn
            .query_row("SELECT COUNT(DISTINCT artist_name) FROM listen_history WHERE artist_name IS NOT NULL", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;

        Ok(DashboardStats {
            total_listens,
            total_duration_ms,
            unique_tracks,
            unique_artists,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    pub total_listens: i64,
    pub total_duration_ms: i64,
    pub unique_tracks: i64,
    pub unique_artists: i64,
}

fn row_to_listen(row: &rusqlite::Row) -> ListenRecord {
    ListenRecord {
        id: row.get(0).ok(),
        track_id: row.get(1).ok().flatten(),
        title: row.get(2).unwrap_or_default(),
        artist_name: row.get(3).ok().flatten(),
        album_title: row.get(4).ok().flatten(),
        source: row.get(5).unwrap_or_else(|_| "local".into()),
        duration_ms: row.get(6).unwrap_or(0),
        listened_at: row.get(7).ok().flatten(),
        zone_id: row.get(8).ok().flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn record_and_query_history() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);
        let rec = ListenRecord {
            id: None,
            track_id: None,
            title: "So What".into(),
            artist_name: Some("Miles Davis".into()),
            album_title: Some("Kind of Blue".into()),
            source: "local".into(),
            duration_ms: 562_000,
            listened_at: None,
            zone_id: None,
        };

        repo.record(&rec).unwrap();
        repo.record(&rec).unwrap();

        let recent = repo.recent(10).unwrap();
        assert_eq!(recent.len(), 2);

        let top = repo.top_tracks(5).unwrap();
        assert_eq!(top[0].0, "So What");
        assert_eq!(top[0].2, 2);

        let dashboard = repo.dashboard().unwrap();
        assert_eq!(dashboard.total_listens, 2);
        assert_eq!(dashboard.unique_tracks, 1);
    }

    #[test]
    fn history_top_artists() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);

        for _ in 0..5 {
            repo.record(&ListenRecord {
                id: None, track_id: None, title: "Song A".into(),
                artist_name: Some("Miles Davis".into()),
                album_title: None, source: "local".into(),
                duration_ms: 300_000, listened_at: None, zone_id: None,
            }).unwrap();
        }
        for _ in 0..3 {
            repo.record(&ListenRecord {
                id: None, track_id: None, title: "Song B".into(),
                artist_name: Some("Coltrane".into()),
                album_title: None, source: "local".into(),
                duration_ms: 400_000, listened_at: None, zone_id: None,
            }).unwrap();
        }

        let top = repo.top_artists(10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "Miles Davis");
        assert_eq!(top[0].1, 5);
        assert_eq!(top[1].0, "Coltrane");
        assert_eq!(top[1].1, 3);
    }

    #[test]
    fn history_top_albums() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);

        for _ in 0..3 {
            repo.record(&ListenRecord {
                id: None, track_id: None, title: "Track".into(),
                artist_name: Some("Miles".into()),
                album_title: Some("Kind of Blue".into()),
                source: "local".into(), duration_ms: 300_000,
                listened_at: None, zone_id: None,
            }).unwrap();
        }

        let top = repo.top_albums(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, "Kind of Blue");
        assert_eq!(top[0].2, 3);
    }

    #[test]
    fn history_count() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);
        assert_eq!(repo.count().unwrap(), 0);

        repo.record(&ListenRecord {
            id: None, track_id: None, title: "A".into(),
            artist_name: None, album_title: None,
            source: "local".into(), duration_ms: 0,
            listened_at: None, zone_id: None,
        }).unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn history_dashboard_total_duration() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);
        repo.record(&ListenRecord {
            id: None, track_id: None, title: "A".into(),
            artist_name: Some("X".into()), album_title: None,
            source: "local".into(), duration_ms: 300_000,
            listened_at: None, zone_id: None,
        }).unwrap();
        repo.record(&ListenRecord {
            id: None, track_id: None, title: "B".into(),
            artist_name: Some("Y".into()), album_title: None,
            source: "tidal".into(), duration_ms: 200_000,
            listened_at: None, zone_id: None,
        }).unwrap();

        let dash = repo.dashboard().unwrap();
        assert_eq!(dash.total_duration_ms, 500_000);
        assert_eq!(dash.unique_tracks, 2);
        assert_eq!(dash.unique_artists, 2);
    }

    #[test]
    fn history_recent_order() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = HistoryRepo::new(db);
        for title in ["First", "Second", "Third"] {
            repo.record(&ListenRecord {
                id: None, track_id: None, title: title.into(),
                artist_name: None, album_title: None,
                source: "local".into(), duration_ms: 0,
                listened_at: None, zone_id: None,
            }).unwrap();
        }

        let recent = repo.recent(10).unwrap();
        assert_eq!(recent.len(), 3);
        // Most recent first
        assert_eq!(recent[0].title, "Third");
    }

    #[test]
    fn history_with_zone_id() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        // Create a zone for the foreign key
        db.execute("INSERT INTO zones (name) VALUES ('Main')", &[]).unwrap();

        let repo = HistoryRepo::new(db);
        repo.record(&ListenRecord {
            id: None, track_id: None, title: "Test".into(),
            artist_name: None, album_title: None,
            source: "local".into(), duration_ms: 100_000,
            listened_at: None, zone_id: Some(1),
        }).unwrap();

        let recent = repo.recent(1).unwrap();
        assert_eq!(recent[0].zone_id, Some(1));
    }
}
