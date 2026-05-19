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
];

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
        if migration.up.is_empty() {
            continue;
        }

        info!(version = migration.version, name = migration.name, "migration_applying");

        db.execute_batch(migration.up)?;

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
