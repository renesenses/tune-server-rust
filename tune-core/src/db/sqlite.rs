use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};
use tracing::info;

use crate::db::engine::{Engine, SqliteDialect};

/// Number of read connections in the pool.
const READ_POOL_SIZE: usize = 3;

pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
    read_pool: Vec<Arc<Mutex<Connection>>>,
    read_counter: Arc<AtomicUsize>,
}

const PRAGMAS_BASE: &str = "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA temp_store=MEMORY;
             PRAGMA mmap_size=268435456;
             PRAGMA analysis_limit=400;";

/// Build the full PRAGMA batch, including adaptive cache_size.
/// Respects `TUNE_CACHE_SIZE` env override (value in negative KB, e.g. `-128000`).
fn build_pragmas() -> String {
    let cache_size = std::env::var("TUNE_CACHE_SIZE")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-64000); // default 64 MB
    format!("{PRAGMAS_BASE}\nPRAGMA cache_size={cache_size};")
}

impl SqliteDb {
    pub fn open(path: &str) -> Result<Self, String> {
        if path == ":memory:" {
            return Self::open_in_memory();
        }

        let pragmas = build_pragmas();

        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(path, flags)
            .map_err(|e| format!("sqlite open {path}: {e}"))?;
        conn.execute_batch(&pragmas)
            .map_err(|e| format!("pragma: {e}"))?;

        // Checkpoint WAL before opening read connections so readers
        // see the latest committed data (prevents stale reads after
        // git reset, crash recovery, or external DB modifications).
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

        // Open a pool of read-only connections for concurrent read access
        let read_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut read_pool = Vec::with_capacity(READ_POOL_SIZE);
        for i in 0..READ_POOL_SIZE {
            let rc = Connection::open_with_flags(path, read_flags)
                .map_err(|e| format!("sqlite open read[{i}] {path}: {e}"))?;
            rc.execute_batch(&pragmas)
                .map_err(|e| format!("pragma read[{i}]: {e}"))?;
            rc.execute_batch("PRAGMA query_only = ON;")
                .map_err(|e| format!("pragma query_only read[{i}]: {e}"))?;
            read_pool.push(Arc::new(Mutex::new(rc)));
        }

        info!(path, readers = READ_POOL_SIZE, "sqlite_opened");

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            read_pool,
            read_counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("sqlite memory: {e}"))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("pragma: {e}"))?;

        // In-memory DBs: share the same connection for reads and writes
        // (separate in-memory connections don't share data)
        let conn = Arc::new(Mutex::new(conn));
        let read_pool = vec![conn.clone(); READ_POOL_SIZE];
        Ok(Self {
            conn,
            read_pool,
            read_counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn connection(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// Returns the next read connection from the round-robin pool.
    pub fn read_connection(&self) -> &Arc<Mutex<Connection>> {
        let idx = self.read_counter.fetch_add(1, Ordering::Relaxed) % self.read_pool.len();
        &self.read_pool[idx]
    }

    pub fn execute(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(sql, params)
            .map_err(|e| format!("execute: {e}"))
    }

    pub fn execute_batch(&self, sql: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql).map_err(|e| format!("batch: {e}"))
    }

    pub fn init_schema(&self) -> Result<(), String> {
        self.execute_batch(CORE_SCHEMA)
    }

    pub fn last_insert_rowid(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.last_insert_rowid()
    }

    /// Execute a read-only closure on the next available read connection.
    pub fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, String> {
        let conn = self.read_connection().lock().unwrap();
        f(&conn).map_err(|e| format!("db read: {e}"))
    }

    /// Execute a write closure on the write connection.
    pub fn write<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, String> {
        let conn = self.conn.lock().unwrap();
        f(&conn).map_err(|e| format!("db write: {e}"))
    }

    pub fn query_timed<T>(&self, label: &str, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self.read_connection().lock().unwrap();
        let start = std::time::Instant::now();
        let result = f(&conn);
        let elapsed = start.elapsed();
        if elapsed > std::time::Duration::from_millis(100) {
            tracing::warn!(query = label, ms = elapsed.as_millis() as u64, "slow_query");
        }
        result
    }

    /// Returns the SQL dialect handle for this backend. Repos that build
    /// engine-agnostic queries use it to emit placeholders, FTS matches
    /// and JSON-extract clauses.
    pub fn dialect(&self) -> SqliteDialect {
        SqliteDialect
    }

    /// Identifies the engine. Always `Engine::Sqlite` for this type.
    pub fn engine(&self) -> Engine {
        Engine::Sqlite
    }
}

impl Clone for SqliteDb {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            read_pool: self.read_pool.clone(),
            read_counter: self.read_counter.clone(),
        }
    }
}

const CORE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS artists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    sort_name TEXT,
    musicbrainz_id TEXT,
    discogs_id TEXT,
    bio TEXT,
    image_path TEXT,
    image_source TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    artist_id INTEGER REFERENCES artists(id),
    year INTEGER,
    original_year INTEGER,
    genre TEXT,
    genres TEXT,
    disc_count INTEGER DEFAULT 1,
    track_count INTEGER DEFAULT 0,
    cover_path TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    label TEXT,
    catalog_number TEXT,
    barcode TEXT,
    format TEXT,
    sample_rate INTEGER,
    bit_depth INTEGER,
    bio TEXT,
    musicbrainz_release_id TEXT,
    musicbrainz_release_group_id TEXT,
    release_date TEXT,
    original_date TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    album_id INTEGER REFERENCES albums(id),
    artist_id INTEGER REFERENCES artists(id),
    album_artist TEXT,
    disc_number INTEGER DEFAULT 1,
    disc_subtitle TEXT,
    track_number INTEGER DEFAULT 0,
    duration_ms INTEGER DEFAULT 0,
    file_path TEXT UNIQUE,
    format TEXT,
    sample_rate INTEGER,
    bit_depth INTEGER,
    channels INTEGER DEFAULT 2,
    file_mtime REAL,
    file_size INTEGER,
    audio_hash TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    isrc TEXT,
    genre TEXT,
    genres TEXT,
    composer TEXT,
    year INTEGER,
    bpm REAL,
    label TEXT,
    musicbrainz_recording_id TEXT,
    comments TEXT
);

CREATE TABLE IF NOT EXISTS track_credits (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_id INTEGER REFERENCES artists(id),
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);
CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_audio_hash ON tracks(audio_hash);
CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id);
CREATE TABLE IF NOT EXISTS playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS zones (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    output_type TEXT,
    output_device_id TEXT,
    volume INTEGER DEFAULT 50,
    muted INTEGER DEFAULT 0,
    online INTEGER DEFAULT 1,
    gapless_enabled INTEGER DEFAULT 1,
    group_id TEXT,
    sync_delay_ms INTEGER NOT NULL DEFAULT 0,
    last_position_ms INTEGER NOT NULL DEFAULT 0,
    last_track_id INTEGER,
    last_track_source TEXT,
    last_track_source_id TEXT,
    max_sample_rate INTEGER,
    fixed_volume INTEGER DEFAULT 0,
    autoplay_enabled INTEGER DEFAULT 0,
    is_hidden INTEGER DEFAULT 0,
    last_play_state TEXT DEFAULT 'stopped',
    dsd_mode TEXT DEFAULT 'auto',
    dlna_native_flac INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS play_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    is_current INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_track_credits_track_id ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_id ON track_credits(artist_id);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist_id ON playlist_tracks(playlist_id);
CREATE INDEX IF NOT EXISTS idx_play_queue_zone_id ON play_queue(zone_id);

-- FTS5 virtual tables for full-text search (accent-insensitive, multi-column)
CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
    title, artist_name, album_title, genre, composer,
    tokenize='unicode61 remove_diacritics 2',
    content='tracks', content_rowid='id'
);
CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
    title, artist_name, genre,
    tokenize='unicode61 remove_diacritics 2',
    content='albums', content_rowid='id'
);
CREATE VIRTUAL TABLE IF NOT EXISTS artists_fts USING fts5(
    name, sort_name,
    tokenize='unicode61 remove_diacritics 2',
    content='artists', content_rowid='id'
);

-- FTS sync triggers: tracks
CREATE TRIGGER IF NOT EXISTS tracks_fts_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer)
    VALUES (new.id, new.title,
            (SELECT name FROM artists WHERE id = new.artist_id),
            (SELECT title FROM albums WHERE id = new.album_id),
            new.genre, new.composer);
END;
CREATE TRIGGER IF NOT EXISTS tracks_fts_update AFTER UPDATE ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title, genre, composer)
    VALUES ('delete', old.id, old.title,
            (SELECT name FROM artists WHERE id = old.artist_id),
            (SELECT title FROM albums WHERE id = old.album_id),
            old.genre, old.composer);
    INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer)
    VALUES (new.id, new.title,
            (SELECT name FROM artists WHERE id = new.artist_id),
            (SELECT title FROM albums WHERE id = new.album_id),
            new.genre, new.composer);
END;
CREATE TRIGGER IF NOT EXISTS tracks_fts_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title, genre, composer)
    VALUES ('delete', old.id, old.title,
            (SELECT name FROM artists WHERE id = old.artist_id),
            (SELECT title FROM albums WHERE id = old.album_id),
            old.genre, old.composer);
END;

-- FTS sync triggers: albums
CREATE TRIGGER IF NOT EXISTS albums_fts_insert AFTER INSERT ON albums BEGIN
    INSERT INTO albums_fts(rowid, title, artist_name, genre)
    VALUES (new.id, new.title,
            (SELECT name FROM artists WHERE id = new.artist_id),
            new.genre);
END;
CREATE TRIGGER IF NOT EXISTS albums_fts_update AFTER UPDATE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, title, artist_name, genre)
    VALUES ('delete', old.id, old.title,
            (SELECT name FROM artists WHERE id = old.artist_id),
            old.genre);
    INSERT INTO albums_fts(rowid, title, artist_name, genre)
    VALUES (new.id, new.title,
            (SELECT name FROM artists WHERE id = new.artist_id),
            new.genre);
END;
CREATE TRIGGER IF NOT EXISTS albums_fts_delete AFTER DELETE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, title, artist_name, genre)
    VALUES ('delete', old.id, old.title,
            (SELECT name FROM artists WHERE id = old.artist_id),
            old.genre);
END;

-- FTS sync triggers: artists
CREATE TRIGGER IF NOT EXISTS artists_fts_insert AFTER INSERT ON artists BEGIN
    INSERT INTO artists_fts(rowid, name, sort_name) VALUES (new.id, new.name, new.sort_name);
END;
CREATE TRIGGER IF NOT EXISTS artists_fts_update AFTER UPDATE ON artists BEGIN
    INSERT INTO artists_fts(artists_fts, rowid, name, sort_name) VALUES ('delete', old.id, old.name, old.sort_name);
    INSERT INTO artists_fts(rowid, name, sort_name) VALUES (new.id, new.name, new.sort_name);
END;
CREATE TRIGGER IF NOT EXISTS artists_fts_delete AFTER DELETE ON artists BEGIN
    INSERT INTO artists_fts(artists_fts, rowid, name, sort_name) VALUES ('delete', old.id, old.name, old.sort_name);
END;
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
    }

    #[test]
    fn schema_creates_tables() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"artists".to_string()));
        assert!(tables.contains(&"albums".to_string()));
        assert!(tables.contains(&"tracks".to_string()));
        assert!(tables.contains(&"track_credits".to_string()));
    }
}
