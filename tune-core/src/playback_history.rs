use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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

fn row_to_entry(r: &[crate::db::backend::SqlValue]) -> HistoryEntry {
    HistoryEntry {
        id: r[0].as_i64().unwrap_or(0),
        track_id: r[1].as_i64(),
        title: r[2].as_string().unwrap_or_default(),
        artist_name: r[3].as_string(),
        album_title: r[4].as_string(),
        source: r[5].as_string().unwrap_or_else(|| "local".into()),
        source_id: r[6].as_string(),
        zone_id: r[7].as_i64().unwrap_or(0),
        played_at: r[8].as_i64().unwrap_or(0),
        duration_ms: r[9].as_i64().unwrap_or(0),
        listened_ms: r[10].as_i64().unwrap_or(0),
    }
}

pub struct PlaybackHistory {
    db: Arc<dyn DbBackend>,
}

impl PlaybackHistory {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        self.db.execute_batch(
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

        use crate::db::backend::ToSqlValue;
        self.db.execute(
            "INSERT INTO playback_history \
             (track_id, title, artist_name, album_title, source, source_id, \
              zone_id, played_at, duration_ms, listened_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            &[
                &track_id as &dyn ToSqlValue,
                &title,
                &artist_name as &dyn ToSqlValue,
                &album_title as &dyn ToSqlValue,
                &source,
                &source_id as &dyn ToSqlValue,
                &zone_id,
                &played_at,
                &duration_ms,
                &listened_ms,
            ],
        )?;

        Ok(self.db.last_insert_rowid())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, String> {
        let lim = limit as i64;
        let rows = self.db.query_many(
            "SELECT id, track_id, title, artist_name, album_title, \
             source, source_id, zone_id, played_at, duration_ms, listened_ms \
             FROM playback_history ORDER BY played_at DESC LIMIT ?1",
            &[&lim],
        )?;
        Ok(rows.iter().map(|r| row_to_entry(r)).collect())
    }

    pub fn since(&self, timestamp: i64) -> Result<Vec<HistoryEntry>, String> {
        let rows = self.db.query_many(
            "SELECT id, track_id, title, artist_name, album_title, \
             source, source_id, zone_id, played_at, duration_ms, listened_ms \
             FROM playback_history WHERE played_at >= ?1 ORDER BY played_at DESC",
            &[&timestamp],
        )?;
        Ok(rows.iter().map(|r| row_to_entry(r)).collect())
    }

    pub fn count(&self) -> Result<i64, String> {
        let row = self
            .db
            .query_one("SELECT COUNT(*) FROM playback_history", &[])?;
        Ok(row.and_then(|r| r[0].as_i64()).unwrap_or(0))
    }

    pub fn top_tracks(
        &self,
        limit: usize,
        since: Option<i64>,
    ) -> Result<Vec<(String, Option<String>, i64)>, String> {
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
        let rows = self.db.query_many(&sql, &[])?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r[0].as_string().unwrap_or_default(),
                    r[1].as_string(),
                    r[2].as_i64().unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.db.execute("DELETE FROM playback_history", &[])?;
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
