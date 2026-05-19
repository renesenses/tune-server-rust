use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, params};
use tracing::{info, warn};

pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDb {
    pub fn open(path: &str) -> Result<Self, String> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;

        let conn = Connection::open_with_flags(path, flags)
            .map_err(|e| format!("sqlite open {path}: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;"
        ).map_err(|e| format!("pragma: {e}"))?;

        info!(path, "sqlite_opened");

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("sqlite memory: {e}"))?;

        conn.execute_batch(
            "PRAGMA foreign_keys=ON;"
        ).map_err(|e| format!("pragma: {e}"))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn connection(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    pub fn execute(&self, sql: &str, params: &[&dyn rusqlite::types::ToSql]) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(sql, params)
            .map_err(|e| format!("execute: {e}"))
    }

    pub fn execute_batch(&self, sql: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql)
            .map_err(|e| format!("batch: {e}"))
    }

    pub fn init_schema(&self) -> Result<(), String> {
        self.execute_batch(CORE_SCHEMA)
    }

    pub fn last_insert_rowid(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.last_insert_rowid()
    }
}

impl Clone for SqliteDb {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
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
    composer TEXT,
    year INTEGER,
    bpm REAL,
    label TEXT,
    musicbrainz_recording_id TEXT
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
    online INTEGER DEFAULT 1
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
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"artists".to_string()));
        assert!(tables.contains(&"albums".to_string()));
        assert!(tables.contains(&"tracks".to_string()));
        assert!(tables.contains(&"track_credits".to_string()));
    }
}
