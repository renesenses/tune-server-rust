use crate::db::sqlite::SqliteDb;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub source: String,
    pub source_id: Option<String>,
    pub zone_id: i64,
    pub played_at: i64,
    pub duration_ms: i64,
    pub listened_ms: i64,
}

pub struct PlaybackHistory {
    db: SqliteDb,
}

impl PlaybackHistory {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS playback_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id INTEGER,
                title TEXT NOT NULL,
                artist_name TEXT,
                album_title TEXT,
                source TEXT NOT NULL DEFAULT 'local',
                source_id TEXT,
                zone_id INTEGER NOT NULL DEFAULT 0,
                played_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                listened_ms INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_ph_played_at ON playback_history(played_at);
            CREATE INDEX IF NOT EXISTS idx_ph_track_id ON playback_history(track_id);",
        )
        .map_err(|e| e.to_string())
    }

    pub fn record(
        &self,
        track_id: Option<i64>,
        title: &str,
        artist_name: Option<&str>,
        album_title: Option<&str>,
        source: &str,
        source_id: Option<&str>,
        zone_id: i64,
        duration_ms: i64,
        listened_ms: i64,
    ) -> Result<i64, String> {
        let played_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO playback_history \
             (track_id, title, artist_name, album_title, source, source_id, \
              zone_id, played_at, duration_ms, listened_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                track_id,
                title,
                artist_name,
                album_title,
                source,
                source_id,
                zone_id,
                played_at,
                duration_ms,
                listened_ms,
            ],
        )
        .map_err(|e| e.to_string())?;

        Ok(conn.last_insert_rowid())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, title, artist_name, album_title, \
                 source, source_id, zone_id, played_at, duration_ms, listened_ms \
                 FROM playback_history ORDER BY played_at DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;

        let entries = stmt
            .query_map([limit as i64], |row| {
                Ok(HistoryEntry {
                    id: row.get(0)?,
                    track_id: row.get(1)?,
                    title: row.get(2)?,
                    artist_name: row.get(3)?,
                    album_title: row.get(4)?,
                    source: row.get(5)?,
                    source_id: row.get(6)?,
                    zone_id: row.get(7)?,
                    played_at: row.get(8)?,
                    duration_ms: row.get(9)?,
                    listened_ms: row.get(10)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(entries)
    }

    pub fn since(&self, timestamp: i64) -> Result<Vec<HistoryEntry>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, title, artist_name, album_title, \
                 source, source_id, zone_id, played_at, duration_ms, listened_ms \
                 FROM playback_history WHERE played_at >= ?1 ORDER BY played_at DESC",
            )
            .map_err(|e| e.to_string())?;

        let entries = stmt
            .query_map([timestamp], |row| {
                Ok(HistoryEntry {
                    id: row.get(0)?,
                    track_id: row.get(1)?,
                    title: row.get(2)?,
                    artist_name: row.get(3)?,
                    album_title: row.get(4)?,
                    source: row.get(5)?,
                    source_id: row.get(6)?,
                    zone_id: row.get(7)?,
                    played_at: row.get(8)?,
                    duration_ms: row.get(9)?,
                    listened_ms: row.get(10)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(entries)
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM playback_history", [], |row| {
            row.get(0)
        })
        .map_err(|e| e.to_string())
    }

    pub fn top_tracks(
        &self,
        limit: usize,
        since: Option<i64>,
    ) -> Result<Vec<(String, Option<String>, i64)>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let sql = if let Some(ts) = since {
            format!(
                "SELECT title, artist_name, COUNT(*) as cnt \
                 FROM playback_history WHERE played_at >= {ts} \
                 GROUP BY title, artist_name ORDER BY cnt DESC LIMIT {limit}"
            )
        } else {
            format!(
                "SELECT title, artist_name, COUNT(*) as cnt \
                 FROM playback_history \
                 GROUP BY title, artist_name ORDER BY cnt DESC LIMIT {limit}"
            )
        };
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let results = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(results)
    }

    pub fn clear(&self) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute("DELETE FROM playback_history", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> SqliteDb {
        let sqlite_db = SqliteDb::open_in_memory().unwrap();
        let history = PlaybackHistory::new(sqlite_db.clone());
        history.setup_table().unwrap();
        sqlite_db
    }

    #[test]
    fn record_and_recent() {
        let db = setup_db();
        let history = PlaybackHistory::new(db);
        let id = history
            .record(
                Some(1),
                "Song A",
                Some("Artist A"),
                Some("Album A"),
                "local",
                None,
                0,
                180_000,
                180_000,
            )
            .unwrap();
        assert!(id > 0);

        let entries = history.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Song A");
    }

    #[test]
    fn count_entries() {
        let db = setup_db();
        let history = PlaybackHistory::new(db);
        assert_eq!(history.count().unwrap(), 0);

        history
            .record(None, "X", None, None, "radio", None, 1, 0, 30_000)
            .unwrap();
        assert_eq!(history.count().unwrap(), 1);
    }

    #[test]
    fn top_tracks_grouping() {
        let db = setup_db();
        let history = PlaybackHistory::new(db);
        for _ in 0..3 {
            history
                .record(
                    Some(1),
                    "Hit",
                    Some("Star"),
                    None,
                    "local",
                    None,
                    0,
                    200_000,
                    200_000,
                )
                .unwrap();
        }
        history
            .record(
                Some(2),
                "Other",
                Some("Star"),
                None,
                "local",
                None,
                0,
                100_000,
                100_000,
            )
            .unwrap();

        let top = history.top_tracks(10, None).unwrap();
        assert_eq!(top[0].0, "Hit");
        assert_eq!(top[0].2, 3);
    }

    #[test]
    fn clear_history() {
        let db = setup_db();
        let history = PlaybackHistory::new(db);
        history
            .record(None, "X", None, None, "local", None, 0, 0, 0)
            .unwrap();
        assert_eq!(history.count().unwrap(), 1);
        history.clear().unwrap();
        assert_eq!(history.count().unwrap(), 0);
    }

    #[test]
    fn since_filter() {
        let db = setup_db();
        let history = PlaybackHistory::new(db);
        history
            .record(None, "Old", None, None, "local", None, 0, 0, 0)
            .unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let entries = history.since(now + 1).unwrap();
        assert!(entries.is_empty());

        let entries = history.since(0).unwrap();
        assert_eq!(entries.len(), 1);
    }
}
