use tracing::info;

use super::sqlite::SqliteDb;

struct Migration {
    version: i32,
    name: &'static str,
    up: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        up: "", // V1 is the CORE_SCHEMA applied by init_schema()
    },
    Migration {
        version: 2,
        name: "add_radio_stations",
        up: "
CREATE TABLE IF NOT EXISTS radio_stations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    homepage TEXT,
    logo_url TEXT,
    country TEXT,
    language TEXT,
    genre TEXT,
    codec TEXT,
    bitrate INTEGER,
    is_favorite INTEGER DEFAULT 0,
    last_played TEXT,
    play_count INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_radio_stations_favorite ON radio_stations(is_favorite);
",
    },
    Migration {
        version: 3,
        name: "add_listen_history",
        up: "
CREATE TABLE IF NOT EXISTS listen_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER REFERENCES tracks(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    artist_name TEXT,
    album_title TEXT,
    source TEXT DEFAULT 'local',
    duration_ms INTEGER DEFAULT 0,
    listened_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    zone_id INTEGER REFERENCES zones(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_listen_history_listened_at ON listen_history(listened_at);
CREATE INDEX IF NOT EXISTS idx_listen_history_track_id ON listen_history(track_id);
",
    },
    Migration {
        version: 4,
        name: "add_settings_table",
        up: "
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
",
    },
    Migration {
        version: 5,
        name: "add_bookmarks",
        up: "
CREATE TABLE IF NOT EXISTS bookmarks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER REFERENCES tracks(id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    label TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_track_id ON bookmarks(track_id);
",
    },
    Migration {
        version: 6,
        name: "add_profiles_favorites_tags_ratings",
        up: "
CREATE TABLE IF NOT EXISTS profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    avatar_path TEXT,
    password_hash TEXT,
    is_admin INTEGER DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id INTEGER NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_favorites_profile ON favorites(profile_id, item_type);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    color TEXT DEFAULT '#808080'
);

CREATE TABLE IF NOT EXISTS item_tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    UNIQUE(tag_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_item_tags_item ON item_tags(item_type, item_id);

CREATE TABLE IF NOT EXISTS album_ratings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    profile_id INTEGER NOT NULL DEFAULT 1,
    rating INTEGER NOT NULL CHECK(rating >= 1 AND rating <= 5),
    note TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(album_id, profile_id)
);
CREATE INDEX IF NOT EXISTS idx_album_ratings_album ON album_ratings(album_id);

CREATE TABLE IF NOT EXISTS smart_playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    sort_by TEXT DEFAULT 'title',
    sort_order TEXT DEFAULT 'asc',
    max_tracks INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

INSERT OR IGNORE INTO profiles (id, username, display_name, is_admin) VALUES (1, 'default', 'Default', 1);
",
    },
    Migration {
        version: 7,
        name: "add_alarms_network_mounts_podcasts",
        up: "
CREATE TABLE IF NOT EXISTS alarms (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER REFERENCES zones(id) ON DELETE CASCADE,
    time TEXT NOT NULL,
    enabled INTEGER DEFAULT 1,
    days TEXT DEFAULT '1,2,3,4,5,6,7',
    source_type TEXT DEFAULT 'playlist',
    source_id INTEGER,
    volume REAL DEFAULT 0.3,
    fade_in_seconds INTEGER DEFAULT 30,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS network_mounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mount_type TEXT NOT NULL DEFAULT 'smb',
    server TEXT NOT NULL,
    share TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    username TEXT,
    password TEXT,
    active INTEGER DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    last_checked TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
",
    },
    Migration {
        version: 8,
        name: "add_radio_favorites_and_alarm_extras",
        up: "
CREATE TABLE IF NOT EXISTS radio_favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    artist TEXT DEFAULT '',
    station_name TEXT DEFAULT '',
    cover_url TEXT,
    stream_url TEXT,
    saved_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(title, artist)
);

ALTER TABLE alarms ADD COLUMN name TEXT DEFAULT 'Alarm';
ALTER TABLE alarms ADD COLUMN one_shot INTEGER DEFAULT 0;
ALTER TABLE alarms ADD COLUMN skip_holidays INTEGER DEFAULT 0;
ALTER TABLE alarms ADD COLUMN source_name TEXT;
ALTER TABLE alarms ADD COLUMN fade_duration_s INTEGER DEFAULT 60;
ALTER TABLE alarms ADD COLUMN last_fired_at DATETIME;
",
    },
    Migration {
        version: 9,
        name: "add_track_credits",
        up: "
CREATE TABLE IF NOT EXISTS track_credits (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL,
    artist_id INTEGER,
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_track_credits_track ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist ON track_credits(artist_name);
",
    },
    Migration {
        version: 10,
        name: "add_album_artist_to_tracks",
        up: "", // Column included in CORE_SCHEMA; for existing DBs, applied programmatically
    },
    Migration {
        version: 11,
        name: "add_genres_column",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 12,
        name: "enhance_fts5_multi_column",
        up: "", // Applied programmatically to rebuild FTS with extra columns
    },
    Migration {
        version: 13,
        name: "add_offline_cache",
        up: "
CREATE TABLE IF NOT EXISTS offline_cache (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    track_title TEXT,
    artist_name TEXT,
    album_title TEXT,
    file_path TEXT,
    file_size INTEGER,
    duration_ms INTEGER,
    quality TEXT,
    downloaded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME,
    status TEXT DEFAULT 'pending',
    error TEXT,
    UNIQUE(source, source_id)
);
CREATE INDEX IF NOT EXISTS idx_offline_cache_source ON offline_cache(source, source_id);
CREATE INDEX IF NOT EXISTS idx_offline_cache_status ON offline_cache(status);
",
    },
];

fn add_column_if_missing(db: &SqliteDb, table: &str, column: &str, col_type: &str) {
    let conn = db.connection().lock().unwrap();
    let has_column = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(1))
                .map(|rows| rows.filter_map(|r| r.ok()).any(|name| name == column))
        })
        .unwrap_or(false);
    drop(conn);
    if !has_column {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {col_type};")).ok();
    }
}

/// Upgrade FTS5 tables from single-column (title only) to multi-column
/// (artist_name, genre, composer, etc.) for richer full-text search.
fn upgrade_fts5_tables(db: &SqliteDb) {
    let sql = "
        -- Drop old triggers
        DROP TRIGGER IF EXISTS tracks_fts_insert;
        DROP TRIGGER IF EXISTS tracks_fts_update;
        DROP TRIGGER IF EXISTS tracks_fts_delete;
        DROP TRIGGER IF EXISTS albums_fts_insert;
        DROP TRIGGER IF EXISTS albums_fts_update;
        DROP TRIGGER IF EXISTS albums_fts_delete;
        DROP TRIGGER IF EXISTS artists_fts_insert;
        DROP TRIGGER IF EXISTS artists_fts_update;
        DROP TRIGGER IF EXISTS artists_fts_delete;

        -- Drop old FTS tables
        DROP TABLE IF EXISTS tracks_fts;
        DROP TABLE IF EXISTS albums_fts;
        DROP TABLE IF EXISTS artists_fts;

        -- Recreate with multiple columns
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

        -- Rebuild (populate from content tables)
        INSERT INTO tracks_fts(tracks_fts) VALUES('rebuild');
        INSERT INTO albums_fts(albums_fts) VALUES('rebuild');
        INSERT INTO artists_fts(artists_fts) VALUES('rebuild');

        -- Auto-sync triggers: tracks
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

        -- Auto-sync triggers: albums
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

        -- Auto-sync triggers: artists
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

    if let Err(e) = db.execute_batch(sql) {
        tracing::warn!(error = %e, "fts5_upgrade_failed");
    } else {
        info!("fts5_tables_upgraded_to_multi_column");
    }
}

pub fn run_migrations(db: &SqliteDb) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )?;

    let current_version = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get::<_, i32>(0),
        ).map_err(|e| e.to_string())?
    };

    let tables_exist = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='artists'",
            [],
            |row| row.get::<_, i32>(0),
        ).map_err(|e| e.to_string())? > 0
    };

    if tables_exist && current_version == 0 {
        db.execute(
            "INSERT OR IGNORE INTO _migrations (version, name) VALUES (?, ?)",
            &[&1i32 as &dyn rusqlite::types::ToSql, &"initial_schema"],
        )?;
        info!(version = 1, "migration_marked_existing");
    }

    for migration in MIGRATIONS {
        if migration.version <= current_version.max(if tables_exist { 1 } else { 0 }) {
            continue;
        }

        info!(version = migration.version, name = migration.name, "migration_applying");

        if !migration.up.is_empty() {
            db.execute_batch(migration.up)?;
        }

        // Programmatic migrations for column additions (safe if column already exists)
        if migration.version == 10 {
            add_column_if_missing(db, "tracks", "album_artist", "TEXT");
        }
        if migration.version == 11 {
            add_column_if_missing(db, "albums", "genres", "TEXT");
            add_column_if_missing(db, "tracks", "genres", "TEXT");
        }
        if migration.version == 12 {
            upgrade_fts5_tables(db);
        }

        db.execute(
            "INSERT INTO _migrations (version, name) VALUES (?, ?)",
            &[&migration.version as &dyn rusqlite::types::ToSql, &migration.name],
        )?;

        info!(version = migration.version, name = migration.name, "migration_applied");
    }

    Ok(())
}

pub fn current_version(db: &SqliteDb) -> Result<i32, String> {
    let has_table = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_migrations'",
            [],
            |row| row.get::<_, i32>(0),
        ).map_err(|e| e.to_string())? > 0
    };

    if !has_table {
        return Ok(0);
    }

    let conn = db.connection().lock().unwrap();
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
        [],
        |row| row.get::<_, i32>(0),
    ).map_err(|e| e.to_string())
}

pub fn latest_version() -> i32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_runs_all_migrations() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        assert_eq!(current_version(&db).unwrap(), latest_version());

        let conn = db.connection().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"radio_stations".to_string()));
        assert!(tables.contains(&"listen_history".to_string()));
        assert!(tables.contains(&"settings".to_string()));
        assert!(tables.contains(&"bookmarks".to_string()));
    }

    #[test]
    fn migrations_are_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        run_migrations(&db).unwrap();
        assert_eq!(current_version(&db).unwrap(), latest_version());
    }

    #[test]
    fn migration_count_matches() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM _migrations",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, latest_version());
    }
}
