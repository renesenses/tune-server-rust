use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::engine::SqlDialect;
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for play_queue_repo.
///
/// The CREATE TABLE for streaming_queue (referenced inline in this
/// repo) is SQLite-specific (INTEGER PRIMARY KEY AUTOINCREMENT). It
/// stays in the impl for now; the phase 3 migration sweep will move it
/// to a portable migration file.
pub mod sql {
    use super::SqlDialect;

    pub fn queue_select_base() -> &'static str {
        "SELECT pq.id, pq.zone_id, pq.track_id, pq.position, pq.is_current, t.title, ar.name, al.title, t.duration_ms, t.file_path, al.cover_path FROM play_queue pq LEFT JOIN tracks t ON pq.track_id = t.id LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE pq.zone_id = {} ORDER BY pq.position",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn get_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE pq.zone_id = {} AND pq.is_current = 1",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn delete_for_zone<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM play_queue WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO play_queue (zone_id, track_id, position, is_current) VALUES ({}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn max_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM play_queue WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row_no_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO play_queue (zone_id, track_id, position, is_current) VALUES ({}, {}, {}, 0)",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn unset_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE play_queue SET is_current = 0 WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_current_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE play_queue SET is_current = 1 WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM play_queue WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_after_delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE play_queue SET position = position - 1 WHERE zone_id = {} AND position > {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_queue WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn insert_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO streaming_queue (zone_id, position, source_id, title, artist, album, cover_url, duration_ms) VALUES ({}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8)
        )
    }

    pub fn select_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT source_id, title, artist, album, cover_url, duration_ms, position FROM streaming_queue WHERE zone_id = {} ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn count_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM play_queue WHERE zone_id = {}",
            d.placeholder(1)
        )
    }
}

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

pub struct PlayQueueRepo {
    db: SqliteDb,
}

impl PlayQueueRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get_queue(&self, zone_id: i64) -> Result<Vec<QueueItem>, String> {
        let query = sql::get_queue(&self.db.dialect());
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![zone_id], |row| Ok(row_to_queue_item(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if !items.is_empty() {
            return Ok(items);
        }
        drop(stmt);
        drop(conn);

        // WAL visibility: the read-only connection may not yet see recently
        // committed rows.  Fall back to the write connection which always has
        // an up-to-date view of its own commits.
        let wconn = self.db.connection().lock().unwrap();
        let mut wstmt = wconn.prepare(&query).map_err(|e| e.to_string())?;
        let items = wstmt
            .query_map(params![zone_id], |row| Ok(row_to_queue_item(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn get_current(&self, zone_id: i64) -> Result<Option<QueueItem>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::get_current(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![zone_id], |row| Ok(row_to_queue_item(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn set_queue(&self, zone_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let d = self.db.dialect();
        let delete_sql = sql::delete_for_zone(&d);
        let insert_sql = sql::insert_queue_row(&d);
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(&delete_sql, params![zone_id])
            .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx.prepare_cached(&insert_sql).map_err(|e| e.to_string())?;
            for (i, tid) in track_ids.iter().enumerate() {
                let is_current = if i == 0 { 1i64 } else { 0i64 };
                stmt.execute(params![zone_id, tid, i as i64, is_current])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn add_tracks(
        &self,
        zone_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<(), String> {
        let d = self.db.dialect();
        let max_pos_sql = sql::max_position(&d);
        let insert_sql = sql::insert_queue_row_no_current(&d);
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let max_pos: i64 = tx
            .query_row(&max_pos_sql, params![zone_id], |row| row.get(0))
            .unwrap_or(-1);
        let start = position.unwrap_or(max_pos + 1);
        {
            let mut stmt = tx.prepare_cached(&insert_sql).map_err(|e| e.to_string())?;
            for (i, tid) in track_ids.iter().enumerate() {
                stmt.execute(params![zone_id, tid, start + i as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_current(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let d = self.db.dialect();
        let unset_sql = sql::unset_current(&d);
        let set_sql = sql::set_current_at(&d);
        let conn = self.db.connection().lock().unwrap();
        conn.execute(&unset_sql, params![zone_id])
            .map_err(|e| e.to_string())?;
        conn.execute(&set_sql, params![zone_id, position])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove_at(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let d = self.db.dialect();
        let delete_sql = sql::delete_at(&d);
        let reindex_sql = sql::reindex_after_delete(&d);
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let deleted = tx
            .execute(&delete_sql, params![zone_id, position])
            .map_err(|e| e.to_string())?;
        if deleted > 0 {
            // Reindex positions so they stay contiguous
            tx.execute(&reindex_sql, params![zone_id, position])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(deleted > 0)
    }

    pub fn clear(&self, zone_id: i64) -> Result<(), String> {
        self.db
            .execute(&sql::delete_for_zone(&self.db.dialect()), &[&zone_id])?;
        // Also clear streaming_queue so GET /queue returns empty for both types
        self.db
            .execute(&sql::delete_streaming(&self.db.dialect()), &[&zone_id])
            .ok();
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn set_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[(String, String, String, Option<String>, Option<String>, i64)],
    ) -> Result<(), String> {
        let d = self.db.dialect();
        let delete_queue_sql = sql::delete_for_zone(&d);
        let delete_streaming_sql = sql::delete_streaming(&d);
        let insert_streaming_sql = sql::insert_streaming(&d);
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(&delete_queue_sql, params![zone_id])
            .map_err(|e| e.to_string())?;
        // SQLite-specific CREATE TABLE (INTEGER PRIMARY KEY AUTOINCREMENT).
        // Phase 3 of the PG roadmap will move this to migrations.
        tx.execute_batch(
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
            )",
        )
        .map_err(|e| e.to_string())?;
        tx.execute(&delete_streaming_sql, params![zone_id])
            .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached(&insert_streaming_sql)
                .map_err(|e| e.to_string())?;
            for (i, (source_id, title, artist, album, cover_url, duration_ms)) in
                tracks.iter().enumerate()
            {
                stmt.execute(params![
                    zone_id,
                    i as i64,
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_streaming_queue(&self, zone_id: i64) -> Result<Vec<serde_json::Value>, String> {
        let select_sql = sql::select_streaming(&self.db.dialect());
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
            )",
        )
        .map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare(&select_sql).map_err(|e| e.to_string())?;
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
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn count(&self, zone_id: i64) -> Result<i64, String> {
        let count_sql = sql::count_queue(&self.db.dialect());
        let conn = self.db.read_connection().lock().unwrap();
        let n: i64 = conn
            .query_row(&count_sql, params![zone_id], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if n > 0 {
            return Ok(n);
        }
        drop(conn);

        // WAL fallback: read connection may lag behind the write connection
        let wconn = self.db.connection().lock().unwrap();
        wconn
            .query_row(&count_sql, params![zone_id], |row| row.get(0))
            .map_err(|e| e.to_string())
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
        db.execute(
            "INSERT INTO zones (name, output_type) VALUES ('Main', 'local')",
            &[],
        )
        .unwrap();
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

    #[test]
    fn queue_add_tracks() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let mut t3 = Track::new("C".into());
        t3.file_path = Some("/c.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.add_tracks(1, &[tid2, tid3], None).unwrap();

        assert_eq!(repo.count(1).unwrap(), 3);

        let queue = repo.get_queue(1).unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].track_id, tid1);
        assert_eq!(queue[1].track_id, tid2);
        assert_eq!(queue[2].track_id, tid3);
    }

    #[test]
    fn queue_add_at_position() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.add_tracks(1, &[tid2], Some(0)).unwrap();

        assert_eq!(repo.count(1).unwrap(), 2);
    }

    #[test]
    fn queue_get_queue_ordered() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut tracks = Vec::new();
        for i in 0..5 {
            let mut t = Track::new(format!("Track {i}"));
            t.file_path = Some(format!("/{i}.flac"));
            let id = track_repo.create(&t).unwrap();
            tracks.push(id);
        }

        repo.set_queue(1, &tracks).unwrap();
        let queue = repo.get_queue(1).unwrap();
        assert_eq!(queue.len(), 5);
        for (i, item) in queue.iter().enumerate() {
            assert_eq!(item.position, i as i64);
            assert_eq!(item.track_id, tracks[i]);
        }
    }

    #[test]
    fn queue_empty_zone() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        let queue = repo.get_queue(1).unwrap();
        assert!(queue.is_empty());
        assert!(repo.get_current(1).unwrap().is_none());
        assert_eq!(repo.count(1).unwrap(), 0);
    }

    #[test]
    fn queue_first_track_is_current() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("First".into());
        t1.file_path = Some("/first.flac".into());
        let mut t2 = Track::new("Second".into());
        t2.file_path = Some("/second.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1, tid2]).unwrap();
        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.track_id, tid1);
        assert!(current.is_current);
    }

    #[test]
    fn queue_streaming_queue() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        let tracks = vec![
            (
                "src-1".into(),
                "Song 1".into(),
                "Artist 1".into(),
                Some("Album 1".into()),
                Some("http://cover1.jpg".into()),
                300_000i64,
            ),
            (
                "src-2".into(),
                "Song 2".into(),
                "Artist 2".into(),
                None,
                None,
                250_000i64,
            ),
        ];

        repo.set_streaming_queue(1, &tracks).unwrap();
        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0]["title"], "Song 1");
        assert_eq!(queue[0]["artist_name"], "Artist 1");
        assert_eq!(queue[0]["duration_ms"], 300_000);
        assert_eq!(queue[1]["title"], "Song 2");
        assert!(queue[1]["album_title"].is_null());
    }

    #[test]
    fn queue_streaming_queue_replace() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        let tracks1 = vec![(
            "id1".into(),
            "Old".into(),
            "Old Artist".into(),
            None,
            None,
            100_000i64,
        )];
        repo.set_streaming_queue(1, &tracks1).unwrap();

        let tracks2 = vec![(
            "id2".into(),
            "New".into(),
            "New Artist".into(),
            None,
            None,
            200_000i64,
        )];
        repo.set_streaming_queue(1, &tracks2).unwrap();

        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["title"], "New");
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        use crate::db::engine::{PostgresDialect, SqliteDialect};
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::insert_queue_row(&s).contains("VALUES (?, ?, ?, ?)"));
        assert!(sql::insert_queue_row(&p).contains("VALUES ($1, $2, $3, $4)"));
        assert!(sql::insert_streaming(&p).contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"));
    }

    #[test]
    fn queue_multiple_zones() {
        let db = test_db();
        db.execute(
            "INSERT INTO zones (name, output_type) VALUES ('Second', 'dlna')",
            &[],
        )
        .unwrap();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.set_queue(2, &[tid2]).unwrap();

        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count(2).unwrap(), 1);

        let q1 = repo.get_queue(1).unwrap();
        assert_eq!(q1[0].track_id, tid1);

        let q2 = repo.get_queue(2).unwrap();
        assert_eq!(q2[0].track_id, tid2);
    }
}
