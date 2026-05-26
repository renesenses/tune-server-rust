use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: i64,
    pub zone_id: i64,
    pub track_id: i64,
    pub position: i64,
    pub is_current: bool,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub duration_ms: Option<i64>,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
}

const SELECT_QUEUE: &str = "SELECT pq.id, pq.zone_id, pq.track_id, pq.position, pq.is_current, t.title, ar.name, al.title, t.duration_ms, t.file_path, al.cover_path FROM play_queue pq LEFT JOIN tracks t ON pq.track_id = t.id LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id";

pub struct PlayQueueRepo {
    db: SqliteDb,
}

impl PlayQueueRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get_queue(&self, zone_id: i64) -> Result<Vec<QueueItem>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_QUEUE} WHERE pq.zone_id = ? ORDER BY pq.position"))
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![zone_id], |row| Ok(row_to_queue_item(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn get_current(&self, zone_id: i64) -> Result<Option<QueueItem>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_QUEUE} WHERE pq.zone_id = ? AND pq.is_current = 1"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![zone_id], |row| Ok(row_to_queue_item(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn set_queue(&self, zone_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        conn.execute("DELETE FROM play_queue WHERE zone_id = ?", params![zone_id])
            .map_err(|e| e.to_string())?;
        for (i, tid) in track_ids.iter().enumerate() {
            let is_current = if i == 0 { 1i64 } else { 0i64 };
            conn.execute(
                "INSERT INTO play_queue (zone_id, track_id, position, is_current) VALUES (?, ?, ?, ?)",
                params![zone_id, tid, i as i64, is_current],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn add_tracks(&self, zone_id: i64, track_ids: &[i64], position: Option<i64>) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        let max_pos: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) FROM play_queue WHERE zone_id = ?",
                params![zone_id],
                |row| row.get(0),
            )
            .unwrap_or(-1);

        let start = position.unwrap_or(max_pos + 1);
        for (i, tid) in track_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO play_queue (zone_id, track_id, position, is_current) VALUES (?, ?, ?, 0)",
                params![zone_id, tid, start + i as i64],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn set_current(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        conn.execute(
            "UPDATE play_queue SET is_current = 0 WHERE zone_id = ?",
            params![zone_id],
        ).map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE play_queue SET is_current = 1 WHERE zone_id = ? AND position = ?",
            params![zone_id, position],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clear(&self, zone_id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM play_queue WHERE zone_id = ?", &[&zone_id])?;
        Ok(())
    }

    pub fn set_streaming_queue(&self, zone_id: i64, tracks: &[(String, String, String, Option<String>, Option<String>, i64)]) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        conn.execute("DELETE FROM play_queue WHERE zone_id = ?", params![zone_id])
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS streaming_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                zone_id INTEGER NOT NULL,
                position INTEGER NOT NULL,
                source TEXT,
                source_id TEXT,
                title TEXT,
                artist TEXT,
                album TEXT,
                cover_url TEXT,
                duration_ms INTEGER DEFAULT 0
            )"
        ).map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM streaming_queue WHERE zone_id = ?", params![zone_id])
            .map_err(|e| e.to_string())?;
        for (i, (source_id, title, artist, album, cover_url, duration_ms)) in tracks.iter().enumerate() {
            conn.execute(
                "INSERT INTO streaming_queue (zone_id, position, source_id, title, artist, album, cover_url, duration_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![zone_id, i as i64, source_id, title, artist, album, cover_url, duration_ms],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn get_streaming_queue(&self, zone_id: i64) -> Result<Vec<serde_json::Value>, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS streaming_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                zone_id INTEGER NOT NULL,
                position INTEGER NOT NULL,
                source TEXT,
                source_id TEXT,
                title TEXT,
                artist TEXT,
                album TEXT,
                cover_url TEXT,
                duration_ms INTEGER DEFAULT 0
            )"
        ).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT source_id, title, artist, album, cover_url, duration_ms, position FROM streaming_queue WHERE zone_id = ? ORDER BY position")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![zone_id], |row| {
                Ok(serde_json::json!({
                    "source_id": row.get::<_, Option<String>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(4).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(5).unwrap_or(0),
                    "position": row.get::<_, i64>(6).unwrap_or(0),
                }))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn count(&self, zone_id: i64) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM play_queue WHERE zone_id = ?",
            params![zone_id],
            |row| row.get(0),
        ).map_err(|e| e.to_string())
    }
}

fn row_to_queue_item(row: &rusqlite::Row) -> QueueItem {
    QueueItem {
        id: row.get(0).unwrap_or(0),
        zone_id: row.get(1).unwrap_or(0),
        track_id: row.get(2).unwrap_or(0),
        position: row.get(3).unwrap_or(0),
        is_current: row.get::<_, i64>(4).unwrap_or(0) != 0,
        title: row.get(5).ok(),
        artist_name: row.get(6).ok(),
        album_title: row.get(7).ok(),
        duration_ms: row.get(8).ok(),
        file_path: row.get(9).ok(),
        cover_path: row.get(10).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Track;
    use crate::db::track_repo::TrackRepo;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.execute("INSERT INTO zones (name, output_type) VALUES ('Main', 'local')", &[]).unwrap();
        db
    }

    #[test]
    fn queue_lifecycle() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("Song 1".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = Track::new("Song 2".into());
        t2.file_path = Some("/2.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1, tid2]).unwrap();
        assert_eq!(repo.count(1).unwrap(), 2);

        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.track_id, tid1);
        assert!(current.is_current);

        repo.set_current(1, 1).unwrap();
        let current2 = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current2.track_id, tid2);

        repo.clear(1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 0);
    }
}
