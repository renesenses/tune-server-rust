//! SQLite → PostgreSQL one-way migration.
//!
//! Gated behind the `postgres` feature (otherwise the subcommand
//! prints an explicit rebuild hint).
//!
//! Strategy:
//! 1. Open SQLite read-only, open PG pool.
//! 2. Walk the 9 core tables in topological order (artists → albums
//!    → tracks → track_credits → playlists → playlist_tracks → zones
//!    → play_queue, plus schema_version).
//! 3. For each table: fetch all rows from SQLite, insert into PG in
//!    batches with `ON CONFLICT DO NOTHING` so re-runs are idempotent.
//! 4. After all tables: COUNT(*) on both sides for verification
//!    (`--skip-verify` to bypass).
//!
//! Limits:
//! - This migrator targets the schema produced by
//!   `tune-core/migrations/postgres/001_initial_schema.sql`. It does
//!   not handle FTS5, additional migrations 2..N, or columns added
//!   after this PR — those will be addressed as PG migrations 002+
//!   land. The set of tables and columns it touches is the
//!   intersection of the 001 schema and what exists on the source.

#![allow(unused_imports, dead_code)]
use std::path::PathBuf;

#[cfg(feature = "postgres")]
mod inner {
    use std::path::Path;
    use std::time::Instant;

    use rusqlite::{Connection, OpenFlags};
    use sqlx::postgres::{PgPool, PgPoolOptions};

    /// Topological order: parents before children (FK dependencies).
    /// Each entry is (table, [columns in INSERT order]).
    ///
    /// `schema_version` is intentionally NOT migrated: the target PG
    /// gets its own version from the bootstrap SQL, and the SQLite
    /// `applied_at` column is stored as TEXT while PG expects
    /// TIMESTAMPTZ — implicit cast doesn't fire through sqlx bind.
    const TABLES: &[(&str, &[&str])] = &[
        (
            "artists",
            &[
                "id",
                "name",
                "sort_name",
                "musicbrainz_id",
                "discogs_id",
                "bio",
                "image_path",
                "image_source",
            ],
        ),
        (
            "albums",
            &[
                "id",
                "title",
                "artist_id",
                "year",
                "original_year",
                "genre",
                "genres",
                "disc_count",
                "track_count",
                "cover_path",
                "source",
                "source_id",
                "label",
                "catalog_number",
                "barcode",
                "format",
                "sample_rate",
                "bit_depth",
                "bio",
                "musicbrainz_release_id",
                "musicbrainz_release_group_id",
                "release_date",
                "original_date",
            ],
        ),
        (
            "tracks",
            &[
                "id",
                "title",
                "album_id",
                "artist_id",
                "album_artist",
                "disc_number",
                "disc_subtitle",
                "track_number",
                "duration_ms",
                "file_path",
                "format",
                "sample_rate",
                "bit_depth",
                "channels",
                "file_mtime",
                "file_size",
                "audio_hash",
                "source",
                "source_id",
                "isrc",
                "genre",
                "genres",
                "composer",
                "year",
                "bpm",
                "label",
                "musicbrainz_recording_id",
            ],
        ),
        (
            "track_credits",
            &[
                "id",
                "track_id",
                "artist_id",
                "artist_name",
                "role",
                "instrument",
                "position",
            ],
        ),
        ("playlists", &["id", "name", "description"]),
        (
            "playlist_tracks",
            &["id", "playlist_id", "track_id", "position"],
        ),
        (
            "zones",
            &[
                "id",
                "name",
                "output_type",
                "output_device_id",
                "volume",
                "muted",
                "online",
                "gapless_enabled",
                "group_id",
                "sync_delay_ms",
                "last_position_ms",
                "last_track_id",
                "last_track_source",
                "last_track_source_id",
            ],
        ),
        (
            "play_queue",
            &["id", "zone_id", "track_id", "position", "is_current"],
        ),
    ];

    pub async fn migrate(
        from: &Path,
        to: &str,
        batch_size: usize,
        only_table: Option<&str>,
        skip_verify: bool,
    ) -> Result<(), String> {
        if batch_size == 0 || batch_size > 10000 {
            return Err("batch_size must be in 1..=10000".into());
        }
        if !from.exists() {
            return Err(format!("source SQLite file not found: {}", from.display()));
        }
        let started = Instant::now();
        println!("Source : {}", from.display());
        println!("Target : {to}");
        println!("Batch  : {batch_size}");
        println!();

        let src = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open sqlite: {e}"))?;
        let pool: PgPool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(to)
            .await
            .map_err(|e| format!("connect postgres: {e}"))?;

        let mut total_inserted = 0u64;

        for (table, cols) in TABLES {
            if let Some(t) = only_table
                && t != *table
            {
                continue;
            }
            let n = migrate_table(&src, &pool, table, cols, batch_size).await?;
            total_inserted += n;
        }

        println!();
        if !skip_verify {
            verify_counts(&src, &pool, only_table).await?;
        } else {
            println!("Verification skipped (--skip-verify).");
        }
        println!(
            "\nDone. {total_inserted} row(s) inserted in {:.1}s.",
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// SQLite type → PG cast hint. The PG schema produced by the
    /// bootstrap script uses BIGINT/BIGSERIAL/SMALLINT for what
    /// SQLite called INTEGER, and DOUBLE PRECISION for REAL.
    fn pg_cast_for(sqlite_type: &str) -> &'static str {
        match sqlite_type.to_uppercase().as_str() {
            "INTEGER" => "bigint",
            "REAL" => "double precision",
            "TEXT" => "text",
            "BLOB" => "bytea",
            _ => "text", // safest default — covers TEXT-affinity columns
        }
    }

    async fn migrate_table(
        src: &Connection,
        pool: &PgPool,
        table: &str,
        cols: &[&str],
        batch_size: usize,
    ) -> Result<u64, String> {
        // Source row count for progress reporting.
        let total: i64 = src
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|e| format!("count {table}: {e}"))?;
        if total == 0 {
            println!("  [skip] {table:20} (empty source)");
            return Ok(0);
        }

        // Discover the SQLite column types so each placeholder carries
        // an explicit ::cast — that way NULL doesn't end up bound as
        // TEXT into a BIGINT column.
        let mut col_type_stmt = src
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| format!("pragma {table}: {e}"))?;
        let info_rows: std::collections::HashMap<String, String> = col_type_stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let typ: String = row.get(2)?;
                Ok((name, typ))
            })
            .map_err(|e| format!("pragma rows {table}: {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("pragma collect {table}: {e}"))?;
        drop(col_type_stmt);

        // Build the static SQL once.
        let col_list = cols.join(", ");
        let placeholders: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let sqlite_type = info_rows.get(*c).map(String::as_str).unwrap_or("TEXT");
                format!("${}::{}", i + 1, pg_cast_for(sqlite_type))
            })
            .collect();
        let pg_insert = format!(
            "INSERT INTO {table} ({col_list}) VALUES ({}) ON CONFLICT DO NOTHING",
            placeholders.join(", ")
        );

        let mut stmt = src
            .prepare(&format!("SELECT {col_list} FROM {table}"))
            .map_err(|e| format!("prepare {table}: {e}"))?;
        let mut rows = stmt.query([]).map_err(|e| format!("query {table}: {e}"))?;

        let mut batch: Vec<Vec<sqlx::types::Json<serde_json::Value>>> =
            Vec::with_capacity(batch_size);
        let mut inserted = 0u64;
        let started = Instant::now();

        while let Some(row) = rows.next().map_err(|e| format!("next {table}: {e}"))? {
            let mut values = Vec::with_capacity(cols.len());
            for i in 0..cols.len() {
                let v: rusqlite::types::Value = row
                    .get(i)
                    .map_err(|e| format!("{table}.{}: {e}", cols[i]))?;
                values.push(sqlx::types::Json(sqlite_to_json(v)));
            }
            batch.push(values);
            if batch.len() >= batch_size {
                inserted += flush(pool, &pg_insert, &batch).await?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            inserted += flush(pool, &pg_insert, &batch).await?;
        }

        let secs = started.elapsed().as_secs_f64();
        println!("  [ok]   {table:20} {inserted:>8} inserted / {total:>8} source ({secs:>5.1}s)");
        Ok(inserted)
    }

    async fn flush(
        pool: &PgPool,
        sql: &str,
        batch: &[Vec<sqlx::types::Json<serde_json::Value>>],
    ) -> Result<u64, String> {
        // We rebind each row one at a time (sqlx doesn't support
        // multi-row binding in a single statement nicely). The batch
        // boundary still buys us the avoidance of round-trips because
        // we wrap in a transaction.
        let mut tx = pool.begin().await.map_err(|e| format!("begin tx: {e}"))?;
        let mut total: u64 = 0;
        for row in batch {
            let mut q = sqlx::query(sql);
            for v in row {
                q = bind_json_value(q, &v.0);
            }
            let res = q
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("insert: {e}"))?;
            total += res.rows_affected();
        }
        tx.commit().await.map_err(|e| format!("commit tx: {e}"))?;
        Ok(total)
    }

    /// Convert a SQLite value into a JSON value, then bind to the
    /// sqlx query as the right Postgres type. We go through JSON so
    /// the type erasure stays simple — for the bootstrap schema
    /// (TEXT, INTEGER, REAL, BLOB) the conversion is lossless.
    fn sqlite_to_json(v: rusqlite::types::Value) -> serde_json::Value {
        use rusqlite::types::Value;
        match v {
            Value::Null => serde_json::Value::Null,
            Value::Integer(i) => serde_json::Value::from(i),
            Value::Real(f) => serde_json::Value::from(f),
            Value::Text(s) => serde_json::Value::String(s),
            Value::Blob(_) => serde_json::Value::Null, // bootstrap schema has no BLOB columns
        }
    }

    fn bind_json_value<'q>(
        q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
        v: &serde_json::Value,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        match v {
            serde_json::Value::Null => q.bind::<Option<&str>>(None),
            serde_json::Value::Bool(b) => q.bind(*b as i64),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    q.bind(i)
                } else if let Some(f) = n.as_f64() {
                    q.bind(f)
                } else {
                    q.bind::<Option<i64>>(None)
                }
            }
            serde_json::Value::String(s) => q.bind(s.clone()),
            _ => q.bind::<Option<&str>>(None),
        }
    }

    async fn verify_counts(
        src: &Connection,
        pool: &PgPool,
        only_table: Option<&str>,
    ) -> Result<(), String> {
        println!("Verifying row counts:");
        for (table, _) in TABLES {
            if let Some(t) = only_table
                && t != *table
            {
                continue;
            }
            let sqlite_count: i64 = src
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .map_err(|e| format!("sqlite count {table}: {e}"))?;
            let pg_count: i64 =
                sqlx::query_scalar(&format!("SELECT COUNT(*)::bigint FROM {table}"))
                    .fetch_one(pool)
                    .await
                    .map_err(|e| format!("pg count {table}: {e}"))?;
            let ok = sqlite_count == pg_count;
            let marker = if ok { "[ok]   " } else { "[diff] " };
            println!("  {marker}{table:20} sqlite={sqlite_count:>8}  pg={pg_count:>8}");
            if !ok {
                return Err(format!(
                    "row-count mismatch on {table}: sqlite={sqlite_count}, pg={pg_count}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(feature = "postgres")]
pub async fn migrate(
    from: &PathBuf,
    to: &str,
    batch_size: usize,
    only_table: Option<&str>,
    skip_verify: bool,
) -> Result<(), String> {
    inner::migrate(from, to, batch_size, only_table, skip_verify).await
}

#[cfg(not(feature = "postgres"))]
pub async fn migrate(
    _from: &PathBuf,
    _to: &str,
    _batch_size: usize,
    _only_table: Option<&str>,
    _skip_verify: bool,
) -> Result<(), String> {
    Err(
        "this tune-cli build does not include the postgres feature.\n  \
         rebuild with: cargo install --path tune-cli --features postgres"
            .into(),
    )
}
