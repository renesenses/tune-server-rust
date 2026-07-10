use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for the unified `queue_items` table (v0.9 rc.2).
///
/// A single table holds both local tracks (`track_id` set, `source='local'`)
/// and streaming tracks (`track_id` NULL, inline metadata). The two subsets are
/// discriminated by `track_id IS [NOT] NULL` and keep independent position
/// spaces, preserving the exact behaviour of the former split
/// `play_queue` / `streaming_queue` tables.
pub mod sql {
    use super::SqlDialect;

    pub fn queue_select_base() -> &'static str {
        "SELECT q.id, q.zone_id, q.track_id, q.position, q.is_current, t.title, ar.name, al.title, t.duration_ms, t.file_path, al.cover_path, t.format, t.sample_rate, t.bit_depth FROM queue_items q LEFT JOIN tracks t ON q.track_id = t.id LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} AND q.track_id IS NOT NULL ORDER BY q.position",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn get_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} AND q.track_id IS NOT NULL AND q.is_current = '1'",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn delete_for_zone<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, {}, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn max_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row_no_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, 0, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn unset_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 0 WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn set_current_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 1 WHERE zone_id = {} AND position = {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_after_delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND track_id IS NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, position, source_id, title, artist, album, cover_url, duration_ms, source) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9)
        )
    }

    pub fn select_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT source_id, title, artist, album, cover_url, duration_ms, position, source FROM queue_items WHERE zone_id = {} AND track_id IS NULL ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn count_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn delete_streaming_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {} AND track_id IS NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_streaming_after_delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {} AND track_id IS NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {} AND track_id IS NULL",
            d.placeholder(1)
        )
    }

    // ─────────────────────────────────────────────────────────────────────
    // Unified single-position-space API (Lot 1 of the queue unification).
    // These builders treat the whole zone queue as ONE ordered sequence,
    // regardless of source (local `track_id` set, streaming `track_id` NULL).
    // Display fields COALESCE the joined track/album/artist (local) with the
    // inline columns (streaming). Added alongside the legacy split builders;
    // callers switch over in Lot 2.
    // ─────────────────────────────────────────────────────────────────────

    pub fn unified_select_base() -> &'static str {
        "SELECT q.id, q.zone_id, q.track_id, q.position, q.is_current, q.source, \
                COALESCE(t.title, q.title), COALESCE(ar.name, q.artist), \
                COALESCE(al.title, q.album), q.source_id, \
                COALESCE(t.duration_ms, q.duration_ms), t.file_path, \
                COALESCE(al.cover_path, q.cover_url), t.format, t.sample_rate, t.bit_depth \
         FROM queue_items q \
         LEFT JOIN tracks t ON q.track_id = t.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_ordered<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} ORDER BY q.position",
            unified_select_base(),
            d.placeholder(1)
        )
    }

    pub fn get_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} AND q.position = {} ORDER BY q.position LIMIT 1",
            unified_select_base(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn max_position_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM queue_items WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    /// Shift positions of every row at/after `from` up by `by`, to open a gap
    /// for an insertion. Placeholders: 1=by, 2=zone_id, 3=from.
    pub fn shift_positions<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position + {} WHERE zone_id = {} AND position >= {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn insert_local_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, 0, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    // insert_streaming (position-explicit) is reused from the legacy builder.

    pub fn delete_at_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_after_delete_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn unset_current_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 0 WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_current_at_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 1 WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_position_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
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
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

/// A queue row in the unified single-position-space model. Unlike `QueueItem`
/// (local-only, `track_id: i64`), this represents BOTH local and streaming
/// items: `track_id`/`file_path` are set for local, `source_id` for streaming,
/// and the display fields are already COALESCE-d from the right origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: i64,
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub position: i64,
    pub is_current: bool,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub duration_ms: Option<i64>,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

impl QueueEntry {
    pub fn is_local(&self) -> bool {
        self.track_id.is_some()
    }
}

/// An item to enqueue, source-agnostic. Used by the unified `insert_at`/`append`.
#[derive(Debug, Clone)]
pub enum QueueInput {
    Local {
        track_id: i64,
    },
    Streaming {
        source: String,
        source_id: String,
        title: String,
        artist: String,
        album: Option<String>,
        cover_url: Option<String>,
        duration_ms: i64,
    },
}

pub struct PlayQueueRepo {
    db: Arc<dyn DbBackend>,
}

impl PlayQueueRepo {
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

    pub fn get_queue(&self, zone_id: i64) -> Result<Vec<QueueItem>, String> {
        // WAL fallback pattern: read first, fall back to strong if 0.
        let sql = self.dialect_sql(sql::get_queue, sql::get_queue);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&sql, &params)?;
        if !rows.is_empty() {
            return Ok(rows.iter().map(row_to_queue_item).collect());
        }
        let strong = self.db.query_many_strong(&sql, &params)?;
        Ok(strong.iter().map(row_to_queue_item).collect())
    }

    pub fn get_current(&self, zone_id: i64) -> Result<Option<QueueItem>, String> {
        let sql = self.dialect_sql(sql::get_current, sql::get_current);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_queue_item))
    }

    pub fn set_queue(&self, zone_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let delete_sql = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        let insert_sql = self.dialect_sql(sql::insert_queue_row, sql::insert_queue_row);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&delete_sql, &p)?;
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = i as i64;
                let is_current = if i == 0 { 1i64 } else { 0i64 };
                let p: [&dyn ToSqlValue; 4] = [&zone_id, tid, &pos, &is_current];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn add_tracks(
        &self,
        zone_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<(), String> {
        let max_pos_sql = self.dialect_sql(sql::max_position, sql::max_position);
        let insert_sql = self.dialect_sql(
            sql::insert_queue_row_no_current,
            sql::insert_queue_row_no_current,
        );
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &p)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start = position.unwrap_or(max_pos + 1);
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = start + i as i64;
                let p: [&dyn ToSqlValue; 3] = [&zone_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    /// Append tracks at the end of the local queue for a zone.
    /// Convenience wrapper over add_tracks(zone_id, track_ids, None).
    pub fn append_tracks(&self, zone_id: i64, track_ids: &[i64]) -> Result<(), String> {
        self.add_tracks(zone_id, track_ids, None)
    }

    pub fn set_current(&self, zone_id: i64, position: i64) -> Result<(), String> {
        // unset-all-then-set-one needs to be atomic — between the two
        // UPDATEs, the zone would have zero "current" entries, which a
        // concurrent read could mistake for an empty queue. write_tx
        // serializes the pair.
        let unset_sql = self.dialect_sql(sql::unset_current, sql::unset_current);
        let set_sql = self.dialect_sql(sql::set_current_at, sql::set_current_at);
        self.db.write_tx(&mut |tx| {
            let p1: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&unset_sql, &p1)?;
            let p2: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            tx.execute(&set_sql, &p2)?;
            Ok(())
        })
    }

    pub fn remove_at(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let delete_sql = self.dialect_sql(sql::delete_at, sql::delete_at);
        let reindex_sql = self.dialect_sql(sql::reindex_after_delete, sql::reindex_after_delete);
        let mut deleted = 0usize;
        let deleted_ref = &mut deleted;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            *deleted_ref = tx.execute(&delete_sql, &p)?;
            if *deleted_ref > 0 {
                tx.execute(&reindex_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(deleted > 0)
    }

    /// Remove a streaming track (track_id NULL) at the given position.
    /// Returns true if a row was actually deleted.
    pub fn remove_streaming_at(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let delete_sql = self.dialect_sql(sql::delete_streaming_at, sql::delete_streaming_at);
        let reindex_sql = self.dialect_sql(
            sql::reindex_streaming_after_delete,
            sql::reindex_streaming_after_delete,
        );
        let mut deleted = 0usize;
        let deleted_ref = &mut deleted;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            *deleted_ref = tx.execute(&delete_sql, &p)?;
            if *deleted_ref > 0 {
                tx.execute(&reindex_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(deleted > 0)
    }

    /// Count streaming tracks (track_id NULL) for a zone.
    pub fn count_streaming(&self, zone_id: i64) -> Result<i64, String> {
        let count_sql = self.dialect_sql(sql::count_streaming, sql::count_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&count_sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n)
    }

    pub fn clear(&self, zone_id: i64) -> Result<(), String> {
        // Delete both subsets (local + streaming) of the unified table.
        let delete_local = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        let delete_streaming = self.dialect_sql(sql::delete_streaming, sql::delete_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        self.db.execute(&delete_local, &params)?;
        self.db.execute(&delete_streaming, &params)?;
        Ok(())
    }

    // ── Unified single-position-space API (Lot 1) ─────────────────────────
    // Added alongside the legacy split methods; route/orchestrator callers
    // switch to these in Lot 2 (with the position-renumbering migration).

    /// Total number of items in the zone queue (local + streaming).
    pub fn count_all(&self, zone_id: i64) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::count_all, sql::count_all);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n)
    }

    /// The whole zone queue as ONE ordered sequence (local + streaming).
    pub fn get_ordered(&self, zone_id: i64) -> Result<Vec<QueueEntry>, String> {
        let sql = self.dialect_sql(sql::get_ordered, sql::get_ordered);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&sql, &params)?;
        if !rows.is_empty() {
            return Ok(rows.iter().map(row_to_queue_entry).collect());
        }
        let strong = self.db.query_many_strong(&sql, &params)?;
        Ok(strong.iter().map(row_to_queue_entry).collect())
    }

    /// The single queue entry at `position` (source-agnostic), if any.
    pub fn get_at(&self, zone_id: i64, position: i64) -> Result<Option<QueueEntry>, String> {
        let sql = self.dialect_sql(sql::get_at, sql::get_at);
        let params: [&dyn ToSqlValue; 2] = [&zone_id, &position];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_queue_entry))
    }

    /// Insert items at `position` (or append when None) in the unified space,
    /// shifting every existing row at/after the insert point up to make room.
    /// This is the basis of "Play Next" (position = current + 1): a streaming
    /// item added while a local album plays now lands right after the current
    /// track instead of at the end of the album (Sandro S1).
    pub fn insert_at(
        &self,
        zone_id: i64,
        items: &[QueueInput],
        position: Option<i64>,
    ) -> Result<(), String> {
        if items.is_empty() {
            return Ok(());
        }
        let max_pos_sql = self.dialect_sql(sql::max_position_any, sql::max_position_any);
        let shift_sql = self.dialect_sql(sql::shift_positions, sql::shift_positions);
        let insert_local_sql = self.dialect_sql(sql::insert_local_at, sql::insert_local_at);
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        let n = items.len() as i64;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &p)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start = position.unwrap_or(max_pos + 1).clamp(0, max_pos + 1);
            // Open a gap of `n` at `start` (no-op when appending at the end).
            let sp: [&dyn ToSqlValue; 3] = [&n, &zone_id, &start];
            tx.execute(&shift_sql, &sp)?;
            for (i, item) in items.iter().enumerate() {
                let pos = start + i as i64;
                match item {
                    QueueInput::Local { track_id } => {
                        let p: [&dyn ToSqlValue; 3] = [&zone_id, track_id, &pos];
                        tx.execute(&insert_local_sql, &p)?;
                    }
                    QueueInput::Streaming {
                        source,
                        source_id,
                        title,
                        artist,
                        album,
                        cover_url,
                        duration_ms,
                    } => {
                        let p: [&dyn ToSqlValue; 9] = [
                            &zone_id,
                            &pos,
                            source_id,
                            title,
                            artist,
                            album,
                            cover_url,
                            duration_ms,
                            source,
                        ];
                        tx.execute(&insert_streaming_sql, &p)?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Append items at the end of the unified queue.
    pub fn append(&self, zone_id: i64, items: &[QueueInput]) -> Result<(), String> {
        self.insert_at(zone_id, items, None)
    }

    /// Remove the item at `position` (source-agnostic) and close the gap.
    pub fn remove_pos(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let delete_sql = self.dialect_sql(sql::delete_at_any, sql::delete_at_any);
        let reindex_sql =
            self.dialect_sql(sql::reindex_after_delete_any, sql::reindex_after_delete_any);
        let mut deleted = 0usize;
        let deleted_ref = &mut deleted;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            *deleted_ref = tx.execute(&delete_sql, &p)?;
            if *deleted_ref > 0 {
                tx.execute(&reindex_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(deleted > 0)
    }

    /// Mark the item at `position` as current (source-agnostic). Unlike the
    /// legacy `set_current` (local rows only), a streaming item can be current.
    pub fn set_current_pos(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let unset_sql = self.dialect_sql(sql::unset_current_any, sql::unset_current_any);
        let set_sql = self.dialect_sql(sql::set_current_at_any, sql::set_current_at_any);
        self.db.write_tx(&mut |tx| {
            let p1: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&unset_sql, &p1)?;
            let p2: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            tx.execute(&set_sql, &p2)?;
            Ok(())
        })
    }

    /// Move the item at `from` to `to` within the unified space, renumbering
    /// the affected rows so positions stay contiguous (0..N-1).
    pub fn move_pos(&self, zone_id: i64, from: i64, to: i64) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        let mut entries = self.get_ordered(zone_id)?;
        let len = entries.len() as i64;
        if from < 0 || from >= len || to < 0 || to >= len {
            return Ok(());
        }
        let item = entries.remove(from as usize);
        entries.insert(to as usize, item);
        let set_pos_sql = self.dialect_sql(sql::set_position_by_id, sql::set_position_by_id);
        self.db.write_tx(&mut |tx| {
            for (i, e) in entries.iter().enumerate() {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 2] = [&pos, &e.id];
                tx.execute(&set_pos_sql, &p)?;
            }
            Ok(())
        })
    }

    #[allow(clippy::type_complexity)]
    pub fn set_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )],
    ) -> Result<(), String> {
        let delete_local_sql = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        let delete_streaming_sql = self.dialect_sql(sql::delete_streaming, sql::delete_streaming);
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&delete_local_sql, &p)?;
            tx.execute(&delete_streaming_sql, &p)?;
            for (i, (source_id, title, artist, album, cover_url, duration_ms, source)) in
                tracks.iter().enumerate()
            {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 9] = [
                    &zone_id,
                    &pos,
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                ];
                tx.execute(&insert_streaming_sql, &p)?;
            }
            Ok(())
        })
    }

    /// Append tracks to the streaming subset for a zone (does NOT clear existing items).
    pub fn append_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )],
    ) -> Result<(), String> {
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        // Get current count to compute starting position for new items
        let current_count = self.count_streaming(zone_id).unwrap_or(0);

        self.db.write_tx(&mut |tx| {
            for (i, (source_id, title, artist, album, cover_url, duration_ms, source)) in
                tracks.iter().enumerate()
            {
                let pos = current_count + i as i64;
                let p: [&dyn ToSqlValue; 9] = [
                    &zone_id,
                    &pos,
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                ];
                tx.execute(&insert_streaming_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn get_streaming_queue(&self, zone_id: i64) -> Result<Vec<serde_json::Value>, String> {
        let select_sql = self.dialect_sql(sql::select_streaming, sql::select_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&select_sql, &params)?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|cols| {
                serde_json::json!({
                    "source_id": cols.first().and_then(|v| v.as_string()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                    "position": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                    "source": cols.get(7).and_then(|v| v.as_string()),
                })
            })
            .collect();
        Ok(items)
    }

    pub fn count(&self, zone_id: i64) -> Result<i64, String> {
        let count_sql = self.dialect_sql(sql::count_queue, sql::count_queue);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&count_sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if n > 0 {
            return Ok(n);
        }
        // WAL fallback: read connection may lag behind the writer.
        let strong = self.db.query_many_strong(&count_sql, &params)?;
        Ok(strong
            .first()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }
}

fn row_to_queue_item(cols: &Vec<SqlValue>) -> QueueItem {
    QueueItem {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        zone_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        track_id: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
        position: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        is_current: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        title: cols.get(5).and_then(|v| v.as_string()),
        artist_name: cols.get(6).and_then(|v| v.as_string()),
        album_title: cols.get(7).and_then(|v| v.as_string()),
        duration_ms: cols.get(8).and_then(|v| v.as_i64()),
        file_path: cols.get(9).and_then(|v| v.as_string()),
        cover_path: cols.get(10).and_then(|v| v.as_string()),
        format: cols.get(11).and_then(|v| v.as_string()),
        sample_rate: cols.get(12).and_then(|v| v.as_i64()),
        bit_depth: cols.get(13).and_then(|v| v.as_i64()),
    }
}

/// Maps a row from `sql::unified_select_base()` (16 columns) to a QueueEntry.
fn row_to_queue_entry(cols: &Vec<SqlValue>) -> QueueEntry {
    QueueEntry {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        zone_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        track_id: cols.get(2).and_then(|v| v.as_i64()),
        position: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        is_current: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        source: cols.get(5).and_then(|v| v.as_string()),
        title: cols.get(6).and_then(|v| v.as_string()),
        artist_name: cols.get(7).and_then(|v| v.as_string()),
        album_title: cols.get(8).and_then(|v| v.as_string()),
        source_id: cols.get(9).and_then(|v| v.as_string()),
        duration_ms: cols.get(10).and_then(|v| v.as_i64()),
        file_path: cols.get(11).and_then(|v| v.as_string()),
        cover_path: cols.get(12).and_then(|v| v.as_string()),
        format: cols.get(13).and_then(|v| v.as_string()),
        sample_rate: cols.get(14).and_then(|v| v.as_i64()),
        bit_depth: cols.get(15).and_then(|v| v.as_i64()),
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
                Some("tidal".into()),
            ),
            (
                "src-2".into(),
                "Song 2".into(),
                "Artist 2".into(),
                None,
                None,
                250_000i64,
                Some("tidal".into()),
            ),
        ];

        repo.set_streaming_queue(1, &tracks).unwrap();
        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0]["title"], "Song 1");
        assert_eq!(queue[0]["artist_name"], "Artist 1");
        assert_eq!(queue[0]["duration_ms"], 300_000);
        assert_eq!(queue[0]["source"], "tidal");
        assert_eq!(queue[1]["title"], "Song 2");
        assert!(queue[1]["album_title"].is_null());
        assert_eq!(queue[1]["source"], "tidal");
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
            Some("qobuz".into()),
        )];
        repo.set_streaming_queue(1, &tracks1).unwrap();

        let tracks2 = vec![(
            "id2".into(),
            "New".into(),
            "New Artist".into(),
            None,
            None,
            200_000i64,
            Some("tidal".into()),
        )];
        repo.set_streaming_queue(1, &tracks2).unwrap();

        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["title"], "New");
        assert_eq!(queue[0]["source"], "tidal");
    }

    #[test]
    fn queue_local_and_streaming_coexist_separately() {
        // Both subsets live in queue_items with independent position spaces.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("Local".into());
        t1.file_path = Some("/l.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        repo.set_queue(1, &[tid1]).unwrap();

        repo.append_streaming_queue(
            1,
            &[(
                "s1".into(),
                "Stream".into(),
                "SA".into(),
                None,
                None,
                123_000i64,
                Some("tidal".into()),
            )],
        )
        .unwrap();

        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count_streaming(1).unwrap(), 1);
        // Local read is unaffected by the streaming row.
        let local = repo.get_queue(1).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].track_id, tid1);
        // clear() removes both subsets.
        repo.clear(1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 0);
        assert_eq!(repo.count_streaming(1).unwrap(), 0);
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::insert_queue_row(&s).contains("VALUES (?, ?, ?, ?, 'local')"));
        assert!(sql::insert_queue_row(&p).contains("VALUES ($1, $2, $3, $4, 'local')"));
        assert!(sql::insert_streaming(&p).contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(sql::get_queue(&s).contains("queue_items"));
        assert!(sql::select_streaming(&s).contains("track_id IS NULL"));
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

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let mut t = Track::new("X".into());
        t.file_path = Some("/x.flac".into());
        let tid = track_repo.create(&t).unwrap();

        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = PlayQueueRepo::with_backend(backend);
        repo.set_queue(1, &[tid]).unwrap();
        assert_eq!(repo.count(1).unwrap(), 1);
    }

    // ── Unified single-position-space API (Lot 1) ─────────────────────────

    fn local(track_id: i64) -> QueueInput {
        QueueInput::Local { track_id }
    }
    fn streaming(id: &str, title: &str) -> QueueInput {
        QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            album: Some("Album".into()),
            cover_url: None,
            duration_ms: 200_000,
        }
    }

    #[test]
    fn unified_append_and_get_ordered_mixed() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t1 = Track::new("L1".into());
        t1.file_path = Some("/1.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();

        repo.append(1, &[local(tid1), streaming("q1", "Q One")])
            .unwrap();
        assert_eq!(repo.count_all(1).unwrap(), 2);

        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].position, 0);
        assert!(q[0].is_local());
        assert_eq!(q[0].track_id, Some(tid1));
        assert_eq!(q[0].title.as_deref(), Some("L1"));
        assert_eq!(q[1].position, 1);
        assert!(!q[1].is_local());
        assert_eq!(q[1].source_id.as_deref(), Some("q1"));
        assert_eq!(q[1].title.as_deref(), Some("Q One"));
        assert_eq!(q[1].source.as_deref(), Some("qobuz"));
    }

    #[test]
    fn unified_play_next_inserts_after_current_s1() {
        // Sandro S1: a local album is playing; adding a Qobuz track via
        // "Play Next" (position = current + 1) must land right after the
        // current track, NOT at the end of the album.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut t = Track::new(format!("L{i}"));
            t.file_path = Some(format!("/{i}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        let locals: Vec<QueueInput> = ids.iter().map(|id| local(*id)).collect();
        repo.append(1, &locals).unwrap();
        // current = position 0 → "Play Next" inserts at position 1.
        repo.insert_at(1, &[streaming("q1", "Q One")], Some(1))
            .unwrap();

        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 4);
        assert_eq!(repo.count_all(1).unwrap(), 4);
        assert_eq!(q[0].track_id, Some(ids[0]));
        assert!(!q[1].is_local(), "Qobuz track must sit right after current");
        assert_eq!(q[1].source_id.as_deref(), Some("q1"));
        assert_eq!(q[2].track_id, Some(ids[1]));
        assert_eq!(q[3].track_id, Some(ids[2]));
        for (i, e) in q.iter().enumerate() {
            assert_eq!(e.position, i as i64, "positions must stay contiguous");
        }
    }

    #[test]
    fn unified_get_at_remove_move_set_current() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t = Track::new("L0".into());
        t.file_path = Some("/0.flac".into());
        let tid = track_repo.create(&t).unwrap();
        repo.append(
            1,
            &[local(tid), streaming("q1", "Q1"), streaming("q2", "Q2")],
        )
        .unwrap();

        // get_at resolves any position, source-agnostic.
        assert_eq!(repo.get_at(1, 0).unwrap().unwrap().track_id, Some(tid));
        assert_eq!(
            repo.get_at(1, 2).unwrap().unwrap().source_id.as_deref(),
            Some("q2")
        );
        assert!(repo.get_at(1, 9).unwrap().is_none());

        // set_current can mark a streaming item (position 1).
        repo.set_current_pos(1, 1).unwrap();
        let cur = repo
            .get_ordered(1)
            .unwrap()
            .into_iter()
            .find(|e| e.is_current)
            .unwrap();
        assert_eq!(cur.source_id.as_deref(), Some("q1"));

        // remove middle → gap closes, positions stay contiguous.
        assert!(repo.remove_pos(1, 1).unwrap());
        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].track_id, Some(tid));
        assert_eq!(q[1].source_id.as_deref(), Some("q2"));
        for (i, e) in q.iter().enumerate() {
            assert_eq!(e.position, i as i64);
        }

        // move the streaming item to the front.
        repo.move_pos(1, 1, 0).unwrap();
        let q2 = repo.get_ordered(1).unwrap();
        assert_eq!(q2[0].source_id.as_deref(), Some("q2"));
        assert_eq!(q2[1].track_id, Some(tid));
        for (i, e) in q2.iter().enumerate() {
            assert_eq!(e.position, i as i64);
        }
    }
}
