//! One-shot SQLite → PostgreSQL data migration.
//!
//! This module reads all rows from the current SQLite `state.db` and
//! copies them into a PostgreSQL database at the provided URL. It is
//! designed as a migration tool, not a live engine switch — Tune
//! continues to run on SQLite after the migration completes.
//!
//! Gated behind `#[cfg(feature = "postgres")]`.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tracing::info;

use super::backend::SqlValue;
use super::sqlite::SqliteDb;

/// Result of a test-connection attempt.
pub struct PgTestResult {
    pub version: String,
}

/// Test a PostgreSQL connection: connect, run SELECT 1, fetch version.
pub async fn test_connection(url: &str) -> Result<PgTestResult, String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    // Verify the connection works
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("SELECT 1 failed: {e}"))?;

    let version: String = sqlx::query_scalar::<_, String>("SELECT version()")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("version query failed: {e}"))?;

    pool.close().await;
    Ok(PgTestResult { version })
}

/// Progress callback for the migration. Called after each table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationProgress {
    pub table: String,
    pub rows_copied: usize,
    pub tables_done: usize,
    pub tables_total: usize,
}

/// Final result of the migration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationResult {
    pub tables_migrated: usize,
    pub total_rows: usize,
    pub errors: Vec<String>,
    pub details: Vec<TableMigrationDetail>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TableMigrationDetail {
    pub table: String,
    pub rows: usize,
    pub skipped: bool,
}

/// Table migration order. Respects foreign key constraints:
/// parents before children.
const MIGRATION_TABLES: &[&str] = &[
    "settings",
    "profiles",
    "artists",
    "albums",
    "tracks",
    "track_credits",
    "track_metadata",
    "playlists",
    "playlist_tracks",
    "zones",
    "play_queue",
    "streaming_queue",
    "listen_history",
    "radio_stations",
    "radio_favorites",
    "tags",
    "item_tags",
    "favorites",
    "album_ratings",
    "smart_playlists",
    "smart_collections",
    "bookmarks",
    "alarms",
    "network_mounts",
    "podcast_subscriptions",
    "offline_cache",
    "sync_links",
    "sync_link_snapshots",
    "track_source_links",
];

/// The complete PG schema DDL. Creates all tables that exist in SQLite.
/// Uses simple types (TEXT/BIGINT/INTEGER/DOUBLE PRECISION) for maximum
/// compatibility with the SQLite data being copied.
///
/// Every CREATE TABLE uses IF NOT EXISTS and every INSERT for seed data
/// uses ON CONFLICT DO NOTHING, making this fully idempotent.
const PG_FULL_SCHEMA: &str = r#"
-- Core tables
CREATE TABLE IF NOT EXISTS artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sort_name TEXT,
    musicbrainz_id TEXT,
    discogs_id TEXT,
    bio TEXT,
    image_path TEXT,
    image_source TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    artist_id TEXT,
    year TEXT,
    original_year TEXT,
    genre TEXT,
    genres TEXT,
    disc_count TEXT DEFAULT 1,
    track_count TEXT DEFAULT 0,
    cover_path TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    label TEXT,
    catalog_number TEXT,
    barcode TEXT,
    format TEXT,
    sample_rate TEXT,
    bit_depth TEXT,
    bio TEXT,
    musicbrainz_release_id TEXT,
    musicbrainz_release_group_id TEXT,
    release_date TEXT,
    original_date TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    album_id TEXT,
    artist_id TEXT,
    album_artist TEXT,
    disc_number TEXT DEFAULT 1,
    disc_subtitle TEXT,
    track_number TEXT DEFAULT 0,
    duration_ms TEXT DEFAULT 0,
    file_path TEXT UNIQUE,
    format TEXT,
    sample_rate TEXT,
    bit_depth TEXT,
    channels TEXT DEFAULT 2,
    file_mtime TEXT,
    file_size TEXT,
    audio_hash TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    isrc TEXT,
    genre TEXT,
    genres TEXT,
    composer TEXT,
    year TEXT,
    bpm TEXT,
    label TEXT,
    musicbrainz_recording_id TEXT,
    comments TEXT,
    waveform_json TEXT,
    acoustid_fingerprint TEXT,
    acoustid_confidence TEXT,
    trailing_silence_ms TEXT,
    synced_lyrics TEXT
);

CREATE TABLE IF NOT EXISTS track_credits (
    id TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    artist_id TEXT,
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS track_metadata (
    track_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (track_id, key)
);

CREATE TABLE IF NOT EXISTS playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    id TEXT PRIMARY KEY,
    playlist_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    position TEXT NOT NULL DEFAULT 0
);

CREATE SEQUENCE IF NOT EXISTS zones_id_seq;
CREATE TABLE IF NOT EXISTS zones (
    id TEXT PRIMARY KEY DEFAULT nextval('zones_id_seq')::text,
    name TEXT NOT NULL,
    output_type TEXT,
    output_device_id TEXT,
    volume TEXT DEFAULT 50,
    muted TEXT DEFAULT 0,
    online TEXT DEFAULT 1,
    gapless_enabled TEXT DEFAULT 1,
    group_id TEXT,
    sync_delay_ms TEXT NOT NULL DEFAULT 0,
    last_position_ms TEXT NOT NULL DEFAULT 0,
    last_track_id TEXT,
    last_track_source TEXT,
    last_track_source_id TEXT,
    max_sample_rate TEXT,
    fixed_volume TEXT DEFAULT 0,
    dsp_preset_id TEXT,
    dsp_enabled TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS play_queue (
    id TEXT PRIMARY KEY,
    zone_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    position TEXT NOT NULL DEFAULT 0,
    is_current TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS streaming_queue (
    id TEXT PRIMARY KEY,
    zone_id TEXT NOT NULL,
    position TEXT NOT NULL,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS listen_history (
    id TEXT PRIMARY KEY,
    track_id TEXT,
    title TEXT NOT NULL,
    artist_name TEXT,
    album_title TEXT,
    source TEXT DEFAULT 'local',
    duration_ms TEXT DEFAULT 0,
    listened_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    zone_id TEXT,
    cover_url TEXT
);

CREATE TABLE IF NOT EXISTS radio_stations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    homepage TEXT,
    logo_url TEXT,
    country TEXT,
    language TEXT,
    genre TEXT,
    codec TEXT,
    bitrate TEXT,
    is_favorite TEXT DEFAULT 0,
    last_played TEXT,
    play_count TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS radio_favorites (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT DEFAULT '',
    station_name TEXT DEFAULT '',
    cover_url TEXT,
    stream_url TEXT,
    saved_at TEXT,
    UNIQUE(title, artist)
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS profiles (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    avatar_path TEXT,
    password_hash TEXT,
    is_admin TEXT DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    email TEXT,
    password_hash_v2 TEXT
);

CREATE TABLE IF NOT EXISTS favorites (
    id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(profile_id, item_type, item_id)
);

CREATE TABLE IF NOT EXISTS tags (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT DEFAULT '#808080'
);

CREATE TABLE IF NOT EXISTS item_tags (
    id TEXT PRIMARY KEY,
    tag_id TEXT NOT NULL,
    item_type TEXT NOT NULL,
    item_id TEXT NOT NULL,
    UNIQUE(tag_id, item_type, item_id)
);

CREATE TABLE IF NOT EXISTS album_ratings (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL,
    profile_id TEXT NOT NULL DEFAULT 1,
    rating TEXT NOT NULL,
    note TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(album_id, profile_id)
);

CREATE TABLE IF NOT EXISTS smart_playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    sort_by TEXT DEFAULT 'title',
    sort_order TEXT DEFAULT 'asc',
    max_tracks TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS smart_collections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    match_mode TEXT NOT NULL DEFAULT 'all',
    sort_by TEXT,
    sort_order TEXT NOT NULL DEFAULT 'asc',
    max_limit TEXT,
    created_at TEXT,
    updated_at TEXT,
    description TEXT,
    icon TEXT,
    color TEXT
);

CREATE TABLE IF NOT EXISTS bookmarks (
    id TEXT PRIMARY KEY,
    track_id TEXT,
    position_ms TEXT NOT NULL DEFAULT 0,
    label TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS alarms (
    id TEXT PRIMARY KEY,
    zone_id TEXT,
    time TEXT NOT NULL,
    enabled TEXT DEFAULT 1,
    days TEXT DEFAULT '1,2,3,4,5,6,7',
    source_type TEXT DEFAULT 'playlist',
    source_id TEXT,
    volume TEXT DEFAULT 0.3,
    fade_in_seconds TEXT DEFAULT 30,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    name TEXT DEFAULT 'Alarm',
    one_shot TEXT DEFAULT 0,
    skip_holidays TEXT DEFAULT 0,
    source_name TEXT,
    fade_duration_s TEXT DEFAULT 60,
    last_fired_at TEXT,
    days_of_week TEXT DEFAULT '1111111',
    multi_zone_ids TEXT
);

CREATE TABLE IF NOT EXISTS network_mounts (
    id TEXT PRIMARY KEY,
    mount_type TEXT NOT NULL DEFAULT 'smb',
    server TEXT NOT NULL,
    share TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    username TEXT,
    password TEXT,
    active TEXT DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id TEXT PRIMARY KEY,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    last_checked TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS offline_cache (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    track_title TEXT,
    artist_name TEXT,
    album_title TEXT,
    file_path TEXT,
    file_size TEXT,
    duration_ms TEXT,
    quality TEXT,
    downloaded_at TEXT,
    expires_at TEXT,
    status TEXT DEFAULT 'pending',
    error TEXT,
    UNIQUE(source, source_id)
);

CREATE TABLE IF NOT EXISTS sync_links (
    id TEXT PRIMARY KEY,
    local_playlist_id TEXT NOT NULL,
    service TEXT NOT NULL,
    remote_playlist_id TEXT NOT NULL,
    direction TEXT NOT NULL DEFAULT 'bidirectional',
    last_synced TEXT,
    created_at TEXT
);

CREATE TABLE IF NOT EXISTS sync_link_snapshots (
    id TEXT PRIMARY KEY,
    playlist_link_id TEXT NOT NULL,
    side TEXT NOT NULL,
    tracks_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS track_source_links (
    id TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    service TEXT NOT NULL,
    service_track_id TEXT NOT NULL,
    confidence TEXT NOT NULL DEFAULT 0.0,
    match_method TEXT,
    linked_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(track_id, service)
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);
CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_audio_hash ON tracks(audio_hash);
CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_track_id ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_id ON track_credits(artist_id);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist_id ON playlist_tracks(playlist_id);
CREATE INDEX IF NOT EXISTS idx_play_queue_zone_id ON play_queue(zone_id);
CREATE INDEX IF NOT EXISTS idx_listen_history_listened_at ON listen_history(listened_at);
CREATE INDEX IF NOT EXISTS idx_listen_history_track_id ON listen_history(track_id);
CREATE INDEX IF NOT EXISTS idx_radio_stations_favorite ON radio_stations(is_favorite);
CREATE INDEX IF NOT EXISTS idx_bookmarks_track_id ON bookmarks(track_id);
CREATE INDEX IF NOT EXISTS idx_favorites_profile ON favorites(profile_id, item_type);
CREATE INDEX IF NOT EXISTS idx_item_tags_item ON item_tags(item_type, item_id);
CREATE INDEX IF NOT EXISTS idx_album_ratings_album ON album_ratings(album_id);
CREATE INDEX IF NOT EXISTS idx_track_metadata_key ON track_metadata(key);
CREATE INDEX IF NOT EXISTS idx_track_source_links_track ON track_source_links(track_id);
CREATE INDEX IF NOT EXISTS idx_track_source_links_service ON track_source_links(service);
CREATE INDEX IF NOT EXISTS idx_offline_cache_source ON offline_cache(source, source_id);
CREATE INDEX IF NOT EXISTS idx_offline_cache_status ON offline_cache(status);
CREATE INDEX IF NOT EXISTS idx_sync_snapshots_link ON sync_link_snapshots(playlist_link_id, side);

-- Schema version tracking
CREATE TABLE IF NOT EXISTS schema_version (
    version TEXT PRIMARY KEY,
    applied_at TIMESTAMPTZ DEFAULT now(),
    name TEXT NOT NULL
);
INSERT INTO schema_version (version, name) VALUES (99, 'sqlite_migration')
    ON CONFLICT (version) DO NOTHING;
"#;

/// Run the full SQLite → PostgreSQL migration.
///
/// 1. Connects to PG at `pg_url`
/// 2. Creates all tables (idempotent)
/// 3. Copies data table by table from `sqlite_db`
/// 4. Returns a summary
pub async fn migrate_sqlite_to_pg(
    sqlite_db: &SqliteDb,
    pg_url: &str,
) -> Result<MigrationResult, String> {
    let start = std::time::Instant::now();

    // Connect to PG
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(15))
        .connect(pg_url)
        .await
        .map_err(|e| format!("pg connect: {e}"))?;

    info!("pg_migrate: connected");

    // Create schema
    sqlx::raw_sql(PG_FULL_SCHEMA)
        .execute(&pool)
        .await
        .map_err(|e| format!("pg schema creation failed: {e}"))?;

    info!("pg_migrate: schema created");

    let mut result = MigrationResult {
        tables_migrated: 0,
        total_rows: 0,
        errors: Vec::new(),
        details: Vec::new(),
    };

    let tables_total = MIGRATION_TABLES.len();

    for (idx, table_name) in MIGRATION_TABLES.iter().enumerate() {
        match migrate_table(sqlite_db, &pool, table_name).await {
            Ok(rows) => {
                info!(
                    table = table_name,
                    rows,
                    progress = format!("{}/{}", idx + 1, tables_total),
                    "pg_migrate_table_done"
                );
                result.tables_migrated += 1;
                result.total_rows += rows;
                result.details.push(TableMigrationDetail {
                    table: table_name.to_string(),
                    rows,
                    skipped: false,
                });
            }
            Err(e) => {
                // Table might not exist in SQLite (e.g. streaming_queue
                // is lazily created). Log and continue.
                let msg = format!("{table_name}: {e}");
                info!(table = table_name, error = %e, "pg_migrate_table_skipped");
                result.errors.push(msg);
                result.details.push(TableMigrationDetail {
                    table: table_name.to_string(),
                    rows: 0,
                    skipped: true,
                });
            }
        }
    }

    // No sequence reset needed — all PKs are TEXT in migration schema

    let elapsed = start.elapsed();
    info!(
        tables = result.tables_migrated,
        rows = result.total_rows,
        errors = result.errors.len(),
        duration_ms = elapsed.as_millis() as u64,
        "pg_migrate_complete"
    );

    pool.close().await;
    Ok(result)
}

/// Migrate a single table from SQLite to PG.
///
/// Reads all rows from SQLite, then inserts them in batches of 1000
/// using ON CONFLICT DO NOTHING for idempotence.
async fn migrate_table(sqlite_db: &SqliteDb, pool: &PgPool, table: &str) -> Result<usize, String> {
    // First, discover the column names from SQLite
    let columns = get_sqlite_columns(sqlite_db, table)?;
    if columns.is_empty() {
        return Err(format!("table {table} has no columns or does not exist"));
    }

    // Read all rows from SQLite
    let col_list = columns.join(", ");
    let sql = format!("SELECT {col_list} FROM {table}");

    let rows: Vec<Vec<SqlValue>> = {
        let conn = sqlite_db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare SELECT from {table}: {e}"))?;
        let col_count = stmt.column_count();
        let mut result_rows = Vec::new();
        let mut query_rows = stmt.query([]).map_err(|e| format!("query {table}: {e}"))?;
        while let Some(row) = query_rows.next().map_err(|e| format!("row {table}: {e}"))? {
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = row
                    .get_ref(i)
                    .map(|vr| match vr {
                        rusqlite::types::ValueRef::Null => SqlValue::Null,
                        rusqlite::types::ValueRef::Integer(n) => SqlValue::Int(n),
                        rusqlite::types::ValueRef::Real(f) => SqlValue::Real(f),
                        rusqlite::types::ValueRef::Text(b) => {
                            SqlValue::Text(String::from_utf8_lossy(b).into_owned())
                        }
                        rusqlite::types::ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
                    })
                    .map_err(|e| format!("col {i} in {table}: {e}"))?;
                vals.push(v);
            }
            result_rows.push(vals);
        }
        result_rows
    };

    if rows.is_empty() {
        return Ok(0);
    }

    let total = rows.len();
    let batch_size = 1000;
    let mut copied = 0;

    // Build the INSERT template. For tables with a composite PK
    // (track_metadata) or a text PK (settings), we need to handle
    // ON CONFLICT differently.
    let conflict_clause = match table {
        "settings" => "ON CONFLICT (key) DO NOTHING",
        "track_metadata" => "ON CONFLICT (track_id, key) DO NOTHING",
        "radio_favorites" => "ON CONFLICT (title, artist) DO NOTHING",
        "favorites" => "ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
        "item_tags" => "ON CONFLICT (tag_id, item_type, item_id) DO NOTHING",
        "album_ratings" => "ON CONFLICT (album_id, profile_id) DO NOTHING",
        "offline_cache" => "ON CONFLICT (source, source_id) DO NOTHING",
        "track_source_links" => "ON CONFLICT (track_id, service) DO NOTHING",
        // For tables with BIGSERIAL PK, conflict on id
        _ => "ON CONFLICT (id) DO NOTHING",
    };

    for chunk in rows.chunks(batch_size) {
        insert_batch(pool, table, &columns, chunk, conflict_clause).await?;
        copied += chunk.len();
        if total > 5000 && copied % 5000 == 0 {
            info!(table, copied, total, "pg_migrate_batch_progress");
        }
    }

    Ok(total)
}

/// Insert a batch of rows into PG using a single multi-row INSERT.
async fn insert_batch(
    pool: &PgPool,
    table: &str,
    columns: &[String],
    rows: &[Vec<SqlValue>],
    conflict_clause: &str,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }

    let col_count = columns.len();

    // Build: INSERT INTO table (col1, col2, ...) VALUES ($1, $2, ...), ($3, $4, ...), ...
    // ON CONFLICT ... DO NOTHING
    let col_list = columns.join(", ");
    let mut sql = format!("INSERT INTO {table} ({col_list}) VALUES ");

    let mut param_idx = 1u32;
    for (row_idx, _row) in rows.iter().enumerate() {
        if row_idx > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for col_idx in 0..col_count {
            if col_idx > 0 {
                sql.push_str(", ");
            }
            sql.push('$');
            sql.push_str(&param_idx.to_string());
            param_idx += 1;
        }
        sql.push(')');
    }
    sql.push(' ');
    sql.push_str(conflict_clause);

    // Bind all values. We use the text-based approach for maximum
    // compatibility: everything goes as TEXT (PG will coerce), except
    // integers and floats which bind natively, and NULLs.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for row in rows {
        for (col_idx, val) in row.iter().enumerate() {
            query = bind_migration_value(
                query,
                val,
                table,
                columns.get(col_idx).map(|s| s.as_str()).unwrap_or("?"),
            );
        }
    }

    query
        .execute(pool)
        .await
        .map_err(|e| format!("INSERT into {table}: {e}"))?;

    Ok(())
}

/// Bind a SqlValue to a sqlx query for the migration.
/// Uses native types where possible to avoid PG type mismatches.
fn bind_migration_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    val: &SqlValue,
    _table: &str,
    _col: &str,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match val {
        SqlValue::Null
        | SqlValue::NullInt
        | SqlValue::NullText
        | SqlValue::NullReal
        | SqlValue::NullBool
        | SqlValue::NullBlob => query.bind(Option::<String>::None),
        SqlValue::Int(i) => query.bind(i.to_string()),
        SqlValue::Real(f) => query.bind(f.to_string()),
        SqlValue::Bool(b) => query.bind(if *b { "1".to_string() } else { "0".to_string() }),
        SqlValue::Text(s) => query.bind(s.clone()),
        SqlValue::Blob(b) => query.bind(b.clone()),
    }
}

/// Get column names for a SQLite table via PRAGMA table_info.
fn get_sqlite_columns(db: &SqliteDb, table: &str) -> Result<Vec<String>, String> {
    let conn = db.read_connection().lock().unwrap();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("pragma table_info({table}): {e}"))?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("table_info query {table}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("table_info collect {table}: {e}"))?;
    Ok(cols)
}

/// After migrating data with explicit IDs, the PG sequences are still
/// at 1. Reset them to MAX(id)+1 so new inserts don't collide.
async fn reset_sequences(pool: &PgPool) -> Result<(), String> {
    // Tables with TEXT PRIMARY KEY named "id"
    let tables = [
        "artists",
        "albums",
        "tracks",
        "track_credits",
        "playlists",
        "playlist_tracks",
        "zones",
        "play_queue",
        "streaming_queue",
        "listen_history",
        "radio_stations",
        "radio_favorites",
        "profiles",
        "favorites",
        "tags",
        "item_tags",
        "album_ratings",
        "smart_playlists",
        "smart_collections",
        "bookmarks",
        "alarms",
        "network_mounts",
        "podcast_subscriptions",
        "offline_cache",
        "sync_links",
        "sync_link_snapshots",
        "track_source_links",
    ];

    for table in &tables {
        let seq_name = format!("{table}_id_seq");
        let sql = format!(
            "SELECT setval('{seq_name}', COALESCE((SELECT MAX(id) FROM {table}), 0) + 1, false)"
        );
        match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
            Ok(_) => {}
            Err(e) => {
                // Sequence might not exist for tables without BIGSERIAL
                info!(table, error = %e, "pg_sequence_reset_skipped");
            }
        }
    }

    Ok(())
}
