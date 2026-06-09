use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Track;
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for track_repo.
///
/// Complex dynamic queries (search() FTS5, list_doubtful() aggregate,
/// deduplicate(), random_ids() with RANDOM()) retain SQLite-specific
/// fragments behind TODO comments; phase 4 swaps them for PG
/// equivalents via dialect helpers.
pub mod sql {
    use super::SqlDialect;

    pub fn select_track() -> &'static str {
        "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, t.album_artist, t.disc_number, t.disc_subtitle, t.track_number, t.duration_ms, t.file_path, t.format, t.sample_rate, t.bit_depth, t.channels, t.file_mtime, t.file_size, t.audio_hash, t.source, t.source_id, t.isrc, t.genre, t.composer, t.year, t.bpm, t.label, t.musicbrainz_recording_id, al.cover_path, t.genres FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("{} WHERE t.id = {}", select_track(), d.placeholder(1))
    }

    pub fn get_by_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.file_path = {}",
            select_track(),
            d.placeholder(1)
        )
    }

    const INSERT_COLS: &str = "title, album_id, artist_id, album_artist, disc_number, disc_subtitle, track_number, duration_ms, file_path, format, sample_rate, bit_depth, channels, file_mtime, file_size, audio_hash, source, source_id, isrc, genre, genres, composer, year, bpm, label, musicbrainz_recording_id";

    pub fn insert<D: SqlDialect>(d: &D) -> String {
        let placeholders: Vec<String> = (1..=26).map(|i| d.placeholder(i)).collect();
        format!(
            "INSERT INTO tracks ({INSERT_COLS}) VALUES ({})",
            placeholders.join(", ")
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET title = {}, album_id = {}, artist_id = {}, album_artist = {}, disc_number = {}, disc_subtitle = {}, track_number = {}, duration_ms = {}, file_path = {}, format = {}, sample_rate = {}, bit_depth = {}, channels = {}, file_mtime = {}, file_size = {}, audio_hash = {}, genre = {}, genres = {}, composer = {}, year = {}, bpm = {}, label = {}, musicbrainz_recording_id = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
            d.placeholder(13),
            d.placeholder(14),
            d.placeholder(15),
            d.placeholder(16),
            d.placeholder(17),
            d.placeholder(18),
            d.placeholder(19),
            d.placeholder(20),
            d.placeholder(21),
            d.placeholder(22),
            d.placeholder(23),
            d.placeholder(24),
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tracks WHERE id = {}", d.placeholder(1))
    }

    pub fn delete_all() -> &'static str {
        "DELETE FROM tracks"
    }

    pub fn delete_by_path<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tracks WHERE file_path = {}", d.placeholder(1))
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM tracks"
    }

    pub fn list_paginated<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} ORDER BY t.id LIMIT {} OFFSET {}",
            select_track(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn list_by_album<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.album_id = {} ORDER BY t.disc_number, t.track_number, t.title",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn list_by_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.artist_id = {} ORDER BY al.year, al.title, t.disc_number, t.track_number",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn list_by_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.file_path = {}",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn update_mtime_and_size<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET file_mtime = {}, file_size = {} WHERE file_path = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn update_audio_hash<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET audio_hash = {} WHERE file_path = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    // ─── Track metadata column helpers (see migration 003) ─────────

    pub fn get_synced_lyrics<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT synced_lyrics FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_synced_lyrics<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET synced_lyrics = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_trailing_silence<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT trailing_silence_ms FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_trailing_silence<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET trailing_silence_ms = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_waveform<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT waveform_json FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_waveform<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET waveform_json = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_acoustid<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET acoustid_fingerprint = {}, acoustid_confidence = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn list_unidentified<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE (t.title LIKE 'Track %' OR t.title LIKE 'Unknown%' \
             OR ar.name = 'Unknown Artist' OR ar.name IS NULL) \
             AND t.acoustid_fingerprint IS NULL \
             AND t.file_path IS NOT NULL \
             ORDER BY t.id LIMIT {}",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn get_credits<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, track_id, artist_id, artist_name, role, instrument, position \
             FROM track_credits WHERE track_id = {} ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn get_all_paths() -> &'static str {
        "SELECT file_path FROM tracks WHERE source = 'local' AND file_path IS NOT NULL"
    }

    pub fn get_all_local_file_info() -> &'static str {
        "SELECT id, file_path, file_mtime, file_size FROM tracks WHERE source = 'local' AND file_path IS NOT NULL"
    }

    pub fn get_existing_audio_hash_album_pairs() -> &'static str {
        "SELECT audio_hash, album_id FROM tracks \
         WHERE source = 'local' AND audio_hash IS NOT NULL AND album_id IS NOT NULL"
    }

    /// Engine-agnostic search.
    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE {} OR LOWER(ar.name) LIKE LOWER({}) OR LOWER(t.genre) LIKE LOWER({}) OR LOWER(t.composer) LIKE LOWER({}) OR CAST(al.year AS TEXT) = {} LIMIT {}",
            select_track(),
            d.fts_where("tracks", "t", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
        )
    }
}

pub struct TrackRepo {
    db: Arc<dyn DbBackend>,
}

impl TrackRepo {
    pub fn backend(&self) -> &dyn DbBackend {
        &*self.db
    }
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

    // ─── Group A: simple ports via DbBackend ──────────────────────

    pub fn get(&self, id: i64) -> Result<Option<Track>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_track))
    }

    pub fn get_by_path(&self, file_path: &str) -> Result<Option<Track>, String> {
        let sql = self.dialect_sql(sql::get_by_path, sql::get_by_path);
        let params: [&dyn ToSqlValue; 1] = [&file_path];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_track))
    }

    pub fn create(&self, track: &Track) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::insert, sql::insert);
        let params: [&dyn ToSqlValue; 26] = [
            &track.title,
            &track.album_id,
            &track.artist_id,
            &track.album_artist,
            &track.disc_number,
            &track.disc_subtitle,
            &track.track_number,
            &track.duration_ms,
            &track.file_path,
            &track.format,
            &track.sample_rate,
            &track.bit_depth,
            &track.channels,
            &track.file_mtime,
            &track.file_size,
            &track.audio_hash,
            &track.source,
            &track.source_id,
            &track.isrc,
            &track.genre,
            &track.genres,
            &track.composer,
            &track.year,
            &track.bpm,
            &track.label,
            &track.musicbrainz_recording_id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn update(&self, track: &Track) -> Result<(), String> {
        let id = track.id.ok_or("track has no id")?;
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 24] = [
            &track.title,
            &track.album_id,
            &track.artist_id,
            &track.album_artist,
            &track.disc_number,
            &track.disc_subtitle,
            &track.track_number,
            &track.duration_ms,
            &track.file_path,
            &track.format,
            &track.sample_rate,
            &track.bit_depth,
            &track.channels,
            &track.file_mtime,
            &track.file_size,
            &track.audio_hash,
            &track.genre,
            &track.genres,
            &track.composer,
            &track.year,
            &track.bpm,
            &track.label,
            &track.musicbrainz_recording_id,
            &id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete_all(&self) -> Result<u64, String> {
        // 4 sequential DELETEs — wrap in write_tx for atomicity.
        let mut count: u64 = 0;
        let count_ref = &mut count;
        self.db.write_tx(&mut |tx| {
            *count_ref = tx.execute(sql::delete_all(), &[])? as u64;
            let _ = tx.execute("DELETE FROM albums", &[]);
            let _ = tx.execute("DELETE FROM artists", &[]);
            let _ = tx.execute("DELETE FROM track_credits", &[]);
            Ok(())
        })?;
        Ok(count)
    }

    pub fn delete_by_path(&self, file_path: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_path, sql::delete_by_path);
        let params: [&dyn ToSqlValue; 1] = [&file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Track>, String> {
        let sql = format!(
            "{} ORDER BY LOWER(ar.name), LOWER(al.title), t.disc_number, t.track_number LIMIT {} OFFSET {}",
            sql::select_track(),
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(1),
                Engine::Postgres => PostgresDialect.placeholder(1),
            },
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(2),
                Engine::Postgres => PostgresDialect.placeholder(2),
            }
        );
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn update_mtime_and_size(
        &self,
        file_path: &str,
        mtime: f64,
        file_size: i64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::update_mtime_and_size, sql::update_mtime_and_size);
        let params: [&dyn ToSqlValue; 3] = [&mtime, &file_size, &file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_audio_hash(&self, file_path: &str, audio_hash: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::update_audio_hash, sql::update_audio_hash);
        let params: [&dyn ToSqlValue; 2] = [&audio_hash, &file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn list_by_album(&self, album_id: i64) -> Result<Vec<Track>, String> {
        let sql = self.dialect_sql(sql::list_by_album, sql::list_by_album);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Track>, String> {
        let sql = self.dialect_sql(sql::list_by_artist, sql::list_by_artist);
        let params: [&dyn ToSqlValue; 1] = [&artist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Track>, String> {
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let trimmed = query.trim();
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 6] = [&fts_query, &like, &like, &like, &trimmed, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn find_by_path(&self, path: &str) -> Result<Option<Track>, String> {
        let sql = self.dialect_sql(sql::get_by_path, sql::get_by_path);
        let params: [&dyn ToSqlValue; 1] = [&path];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_track))
    }

    pub fn search_by_title(&self, title: &str, limit: i64) -> Result<Vec<Track>, String> {
        let like = format!("%{title}%");
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "{} WHERE LOWER(t.title) LIKE LOWER({}) LIMIT {}",
            sql::select_track(),
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn exists_by_audio_hash_and_album(
        &self,
        audio_hash: &str,
        album_id: i64,
    ) -> Result<bool, String> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "SELECT COUNT(*) FROM tracks WHERE audio_hash = {} AND album_id = {}",
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&audio_hash, &album_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n > 0)
    }

    pub fn random_ids(&self, limit: i64) -> Result<Vec<i64>, String> {
        // Both engines accept `ORDER BY RANDOM()` (SQLite) /
        // `ORDER BY random()` (PG). The lowercase form works on both.
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "SELECT id FROM tracks ORDER BY random() LIMIT {}",
            make_ph(1)
        );
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn count_doubtful(&self) -> Result<i64, String> {
        let sql = format!(
            "SELECT COUNT(*) FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE (ar.name IS NULL OR ar.name = '' OR ar.name = 'Unknown Artist') \
                OR (t.duration_ms > 0 AND t.duration_ms < 5000) \
                OR (al.title IS NULL OR al.title = '')"
        );
        Ok(self
            .db
            .query_one(&sql, &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }

    pub fn list_doubtful(&self, limit: i64, offset: i64) -> Result<Vec<Track>, String> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "{} \
             WHERE (ar.name IS NULL OR ar.name = '' OR ar.name = 'Unknown Artist') \
                OR (t.duration_ms > 0 AND t.duration_ms < 5000) \
                OR (al.title IS NULL OR al.title = '') \
             ORDER BY t.id LIMIT {} OFFSET {}",
            sql::select_track(),
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn get_multiple(&self, ids: &[i64]) -> Result<Vec<Track>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let placeholders: Vec<String> = (1..=ids.len()).map(make_ph).collect();
        let sql = format!(
            "{} WHERE t.id IN ({})",
            sql::select_track(),
            placeholders.join(",")
        );
        let owned: Vec<SqlValue> = ids.iter().map(|id| SqlValue::Int(*id)).collect();
        let refs: Vec<&dyn ToSqlValue> = owned.iter().map(|v| v as &dyn ToSqlValue).collect();
        let rows = self.db.query_many(&sql, &refs)?;
        let tracks: Vec<Track> = rows.iter().map(row_to_track).collect();
        // Preserve caller's ordering
        let mut ordered = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = tracks.iter().find(|t| t.id == Some(*id)) {
                ordered.push(t.clone());
            }
        }
        Ok(ordered)
    }

    // ─── Group B/C: write_tx + simple inline ──────────────────────

    /// Insert multiple tracks using individual execute calls.
    ///
    /// **Important**: this method does NOT start its own transaction.
    /// The caller is responsible for wrapping the call in a transaction
    /// (e.g. `BEGIN IMMEDIATE` / `COMMIT`) if atomicity is needed.
    /// Using `write_tx` here would fail with "cannot start a transaction
    /// within a transaction" when the caller already holds one.
    pub fn create_batch(&self, tracks: &[Track]) -> Result<usize, String> {
        let insert_sql = self.dialect_sql(sql::insert, sql::insert);
        let mut count = 0usize;
        for track in tracks {
            let params: [&dyn ToSqlValue; 26] = [
                &track.title,
                &track.album_id,
                &track.artist_id,
                &track.album_artist,
                &track.disc_number,
                &track.disc_subtitle,
                &track.track_number,
                &track.duration_ms,
                &track.file_path,
                &track.format,
                &track.sample_rate,
                &track.bit_depth,
                &track.channels,
                &track.file_mtime,
                &track.file_size,
                &track.audio_hash,
                &track.source,
                &track.source_id,
                &track.isrc,
                &track.genre,
                &track.genres,
                &track.composer,
                &track.year,
                &track.bpm,
                &track.label,
                &track.musicbrainz_recording_id,
            ];
            if self.db.execute(&insert_sql, &params).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Update multiple tracks using individual execute calls.
    ///
    /// **Important**: this method does NOT start its own transaction.
    /// The caller is responsible for wrapping the call in a transaction
    /// (e.g. `BEGIN IMMEDIATE` / `COMMIT`) if atomicity is needed.
    /// See `create_batch` for rationale.
    pub fn update_batch(&self, tracks: &[Track]) -> Result<usize, String> {
        let update_sql = self.dialect_sql(sql::update, sql::update);
        let mut count = 0usize;
        for track in tracks {
            let Some(id) = track.id else { continue };
            let params: [&dyn ToSqlValue; 24] = [
                &track.title,
                &track.album_id,
                &track.artist_id,
                &track.album_artist,
                &track.disc_number,
                &track.disc_subtitle,
                &track.track_number,
                &track.duration_ms,
                &track.file_path,
                &track.format,
                &track.sample_rate,
                &track.bit_depth,
                &track.channels,
                &track.file_mtime,
                &track.file_size,
                &track.audio_hash,
                &track.genre,
                &track.genres,
                &track.composer,
                &track.year,
                &track.bpm,
                &track.label,
                &track.musicbrainz_recording_id,
                &id,
            ];
            if self.db.execute(&update_sql, &params).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    // ─── Group B: metadata accessors via DbBackend ───────────────────
    // Backed by migration `003_track_metadata_columns.sql` on PG.

    pub fn get_synced_lyrics(&self, track_id: i64) -> Result<Option<String>, String> {
        let sql = self.dialect_sql(sql::get_synced_lyrics, sql::get_synced_lyrics);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_string())))
    }

    pub fn set_synced_lyrics(&self, track_id: i64, json: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::set_synced_lyrics, sql::set_synced_lyrics);
        let params: [&dyn ToSqlValue; 2] = [&json, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_trailing_silence(&self, track_id: i64) -> Result<Option<i64>, String> {
        let sql = self.dialect_sql(sql::get_trailing_silence, sql::get_trailing_silence);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64())))
    }

    pub fn set_trailing_silence(&self, track_id: i64, ms: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::set_trailing_silence, sql::set_trailing_silence);
        let params: [&dyn ToSqlValue; 2] = [&ms, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn set_acoustid(
        &self,
        track_id: i64,
        fingerprint: &str,
        confidence: f64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::set_acoustid, sql::set_acoustid);
        let params: [&dyn ToSqlValue; 3] = [&fingerprint, &confidence, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn list_unidentified(&self, limit: i64) -> Result<Vec<Track>, String> {
        let sql = self.dialect_sql(sql::list_unidentified, sql::list_unidentified);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn get_waveform(&self, track_id: i64) -> Result<Option<String>, String> {
        let sql = self.dialect_sql(sql::get_waveform, sql::get_waveform);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_string())))
    }

    pub fn set_waveform(&self, track_id: i64, json: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::set_waveform, sql::set_waveform);
        let params: [&dyn ToSqlValue; 2] = [&json, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_credits(
        &self,
        track_id: i64,
    ) -> Result<Vec<crate::db::models::TrackCredit>, String> {
        let sql = self.dialect_sql(sql::get_credits, sql::get_credits);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| crate::db::models::TrackCredit {
                id: cols.first().and_then(|v| v.as_i64()),
                track_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                artist_id: cols.get(2).and_then(|v| v.as_i64()),
                artist_name: cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                role: cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                instrument: cols.get(5).and_then(|v| v.as_string()),
                position: cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            })
            .collect())
    }

    pub fn get_all_paths(&self) -> Result<HashSet<String>, String> {
        let rows = self.db.query_many(sql::get_all_paths(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
            .collect())
    }

    #[allow(clippy::type_complexity)]
    pub fn get_all_local_file_info(
        &self,
    ) -> Result<HashMap<String, (i64, Option<f64>, Option<i64>)>, String> {
        let rows = self.db.query_many(sql::get_all_local_file_info(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| {
                let id = cols.first().and_then(|v| v.as_i64())?;
                let path = cols.get(1).and_then(|v| v.as_string())?;
                let mtime = cols.get(2).and_then(|v| v.as_f64());
                let size = cols.get(3).and_then(|v| v.as_i64());
                Some((path, (id, mtime, size)))
            })
            .collect())
    }

    /// List all local tracks (with file_path set). Used by rescan-metadata to
    /// re-read tags from disk without doing a full library scan.
    pub fn list_all_local(&self) -> Result<Vec<Track>, String> {
        let sql = format!(
            "{} WHERE t.file_path IS NOT NULL AND t.file_path != ''",
            sql::select_track()
        );
        let rows = self.db.query_many(&sql, &[])?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn get_existing_audio_hash_album_pairs(&self) -> Result<HashSet<(String, i64)>, String> {
        let rows = self
            .db
            .query_many(sql::get_existing_audio_hash_album_pairs(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| {
                let hash = cols.first().and_then(|v| v.as_string())?;
                let album_id = cols.get(1).and_then(|v| v.as_i64())?;
                Some((hash, album_id))
            })
            .collect())
    }

    pub fn deduplicate(&self) -> Result<i64, String> {
        let count_sql = "SELECT COUNT(*) FROM tracks t1 WHERE EXISTS (SELECT 1 FROM tracks t2 WHERE t2.audio_hash = t1.audio_hash AND t2.id < t1.id AND t1.audio_hash IS NOT NULL)";
        let delete_sql = "DELETE FROM tracks WHERE id IN (SELECT t1.id FROM tracks t1 WHERE EXISTS (SELECT 1 FROM tracks t2 WHERE t2.audio_hash = t1.audio_hash AND t2.id < t1.id AND t1.audio_hash IS NOT NULL))";
        let mut count: i64 = 0;
        let count_ref = &mut count;
        self.db.write_tx(&mut |tx| {
            *count_ref = tx
                .query_one(count_sql, &[])?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if *count_ref > 0 {
                tx.execute(delete_sql, &[])?;
            }
            Ok(())
        })?;
        Ok(count)
    }
}

fn row_to_track(cols: &Vec<SqlValue>) -> Track {
    Track {
        id: cols.first().and_then(|v| v.as_i64()),
        title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        album_id: cols.get(2).and_then(|v| v.as_i64()),
        album_title: cols.get(3).and_then(|v| v.as_string()),
        artist_id: cols.get(4).and_then(|v| v.as_i64()),
        artist_name: cols.get(5).and_then(|v| v.as_string()),
        album_artist: cols.get(6).and_then(|v| v.as_string()),
        disc_number: cols.get(7).and_then(|v| v.as_i64()).unwrap_or(1) as i32,
        disc_subtitle: cols.get(8).and_then(|v| v.as_string()),
        track_number: cols.get(9).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        duration_ms: cols.get(10).and_then(|v| v.as_i64()).unwrap_or(0),
        file_path: cols.get(11).and_then(|v| v.as_string()),
        format: cols.get(12).and_then(|v| v.as_string()),
        sample_rate: cols.get(13).and_then(|v| v.as_i64()).map(|n| n as i32),
        bit_depth: cols.get(14).and_then(|v| v.as_i64()).map(|n| n as i32),
        channels: cols.get(15).and_then(|v| v.as_i64()).unwrap_or(2) as i32,
        file_mtime: cols.get(16).and_then(|v| v.as_f64()),
        file_size: cols.get(17).and_then(|v| v.as_i64()),
        audio_hash: cols.get(18).and_then(|v| v.as_string()),
        source: cols
            .get(19)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "local".into()),
        source_id: cols.get(20).and_then(|v| v.as_string()),
        isrc: cols.get(21).and_then(|v| v.as_string()),
        genre: cols.get(22).and_then(|v| v.as_string()),
        composer: cols.get(23).and_then(|v| v.as_string()),
        year: cols.get(24).and_then(|v| v.as_i64()).map(|n| n as i32),
        bpm: cols.get(25).and_then(|v| v.as_f64()),
        label: cols.get(26).and_then(|v| v.as_string()),
        musicbrainz_recording_id: cols.get(27).and_then(|v| v.as_string()),
        cover_path: cols.get(28).and_then(|v| v.as_string()),
        genres: cols.get(29).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn crud_track() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("DSOTM", aid, Some(1973))
            .unwrap()
            .id
            .unwrap();

        let mut track = Track::new("Time".into());
        track.artist_id = Some(aid);
        track.album_id = Some(alid);
        track.file_path = Some("/music/pink_floyd/dsotm/time.flac".into());
        track.duration_ms = 413000;
        track.sample_rate = Some(44100);
        track.bit_depth = Some(16);

        let id = repo.create(&track).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "Time");
        assert_eq!(fetched.artist_name.as_deref(), Some("Pink Floyd"));
        assert_eq!(fetched.album_title.as_deref(), Some("DSOTM"));

        let by_path = repo
            .get_by_path("/music/pink_floyd/dsotm/time.flac")
            .unwrap();
        assert!(by_path.is_some());

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn get_all_paths() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Song 1".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("Song 2".into());
        t2.file_path = Some("/b.flac".into());

        repo.create(&t1).unwrap();
        repo.create(&t2).unwrap();

        let paths = repo.get_all_paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("/a.flac"));
    }

    #[test]
    fn get_multiple_preserves_order() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Alpha".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = Track::new("Beta".into());
        t2.file_path = Some("/2.flac".into());
        let mut t3 = Track::new("Gamma".into());
        t3.file_path = Some("/3.flac".into());

        let id1 = repo.create(&t1).unwrap();
        let id2 = repo.create(&t2).unwrap();
        let id3 = repo.create(&t3).unwrap();

        let result = repo.get_multiple(&[id3, id1, id2]).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].title, "Gamma");
        assert_eq!(result[1].title, "Alpha");
        assert_eq!(result[2].title, "Beta");
    }

    #[test]
    fn search_tracks() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Comfortably Numb".into());
        t.file_path = Some("/numb.flac".into());
        repo.create(&t).unwrap();

        let results = repo.search("comfort", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn track_count() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        let mut t = Track::new("A".into());
        t.file_path = Some("/a.flac".into());
        repo.create(&t).unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn with_backend_constructor_full() {
        // All methods now go through DbBackend — no more sqlite_legacy
        // fallback. The with_backend path is fully functional on SQLite.
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = TrackRepo::with_backend(backend);
        let mut t = Track::new("X".into());
        t.file_path = Some("/x.flac".into());
        let id = repo.create(&t).unwrap();
        assert!(repo.get(id).unwrap().is_some());
        // Methods previously requiring sqlite_legacy now work via
        // DbBackend. get_all_paths reads from the base schema.
        assert!(repo.get_all_paths().unwrap().contains("/x.flac"));
        assert!(
            repo.get_existing_audio_hash_album_pairs()
                .unwrap()
                .is_empty()
        );
    }
}
