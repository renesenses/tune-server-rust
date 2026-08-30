//! PostgreSQL backend (phase 2 of the PG support roadmap).
//!
//! Gated by the `postgres` feature flag. This file holds the connection
//! pool primitives — repo migration follows in later phases.
//!
//! See docs/POSTGRES-PLAN.md.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tracing::{info, warn};

use crate::db::engine::{Engine, PostgresDialect};

// Whole TABLES added to PG_FULL_SCHEMA after a DB was first migrated:
// PG_FULL_SCHEMA only runs during the one-time SQLite→PG migration, so a
// table introduced later (e.g. `file_first_seen`, #473) never lands on an
// already-migrated DB. The albums list sorts "added_at" via a LEFT JOIN
// on file_first_seen, so its absence made the query fail on .15 prod
// (`relation "file_first_seen" does not exist`) → empty library → the
// "black screen" reported on the iOS/Android clients. Same class again in
// v0.9: the unified `queue_items` table (replacing play_queue/streaming_queue)
// was added to PG_FULL_SCHEMA only, so .15 prod raised `relation "queue_items"
// does not exist` on set_queue → no sound. Its CREATE is included below.
// NOTE the column types: the unified-queue queries do integer arithmetic on
// `position`/`is_current` (`position - 1`, `COALESCE(MAX(position), -1)`,
// `is_current = 1`, `ORDER BY position`), which PG rejects on TEXT columns
// (`COALESCE types text and integer cannot be matched`). The legacy play_queue /
// streaming_queue on .15 prod are BIGINT, so queue_items must be BIGINT too. No
// data copy here: this runs every startup, and an on-empty copy would resurrect a
// stale queue from play_queue/streaming_queue after a user clears their queue. The
// one-time copy lives in the SQLite→PG migrator (pg_migrate.rs) instead.
// CREATE TABLE IF NOT EXISTS is idempotent; add new tables here like the columns below.
//
// ONE STATEMENT PER ENTRY, and one round-trip each (see `run_each`).
// These used to be a single `;`-joined string handed to `raw_sql`, which
// PostgreSQL runs through the simple query protocol — every statement in
// ONE implicit transaction. A single failing statement therefore rolled
// back the whole batch, including the statements that had already
// succeeded and every statement after it. That is exactly what bricked
// .15 prod (#1706): `streaming_favorites.id` is BIGINT there (migration
// 012 converts the migrated TEXT ids back to bigint + sequence), so the
// unconditional `SET DEFAULT nextval(...)::text` below raised
//   column "id" is of type bigint but default expression is of type text
// and took the whole batch down with it — `pg_ensure_tables_failed`, once
// per boot. A table that fails must never block the next one.
pub(crate) const ENSURE_TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS file_first_seen (file_path TEXT PRIMARY KEY, first_seen_at DOUBLE PRECISION NOT NULL)",
    "CREATE SEQUENCE IF NOT EXISTS streaming_favorites_id_seq",
    "CREATE TABLE IF NOT EXISTS streaming_favorites (\
            id TEXT PRIMARY KEY DEFAULT nextval('streaming_favorites_id_seq')::text,\
            profile_id TEXT NOT NULL DEFAULT '1',\
            item_type TEXT NOT NULL,\
            service TEXT NOT NULL,\
            service_id TEXT NOT NULL,\
            title TEXT,\
            artist TEXT,\
            album TEXT,\
            cover_url TEXT,\
            created_at TEXT,\
            UNIQUE(profile_id, item_type, service, service_id)\
        )",
    // Only re-attach the TEXT default while the column IS still text.
    // On a database healed by migration 012 the column is BIGINT and
    // already defaults to `nextval('streaming_favorites_id_seq')`, so
    // forcing the `::text` flavour is both wrong and fatal (#1706).
    "DO $ensure$ BEGIN \
            IF EXISTS (SELECT 1 FROM information_schema.columns \
                        WHERE table_name = 'streaming_favorites' AND column_name = 'id' \
                          AND data_type IN ('text', 'character varying')) THEN \
                ALTER TABLE streaming_favorites \
                    ALTER COLUMN id SET DEFAULT nextval('streaming_favorites_id_seq')::text; \
            END IF; \
        END $ensure$",
    "CREATE SEQUENCE IF NOT EXISTS queue_items_id_seq",
    // track_number/disc_number carry the per-album numbering of streaming
    // tracks (SQLite migration v64). They were added to PG_FULL_SCHEMA and
    // to the SQLite schema but NOT here, while `insert_streaming`,
    // `select_streaming` and `unified_select_base` all name them — so every
    // queue write on a database whose queue_items came from this CREATE
    // failed with `column "track_number" of relation "queue_items" does not
    // exist`, and no queue was ever restored at boot (#1706, 9 zones on .15).
    "CREATE TABLE IF NOT EXISTS queue_items (\
            id BIGINT PRIMARY KEY DEFAULT nextval('queue_items_id_seq'),\
            zone_id BIGINT NOT NULL,\
            position BIGINT NOT NULL DEFAULT 0,\
            is_current BIGINT DEFAULT 0,\
            track_id BIGINT,\
            source TEXT,\
            source_id TEXT,\
            title TEXT,\
            artist TEXT,\
            album TEXT,\
            cover_url TEXT,\
            duration_ms BIGINT DEFAULT 0,\
            track_number BIGINT,\
            disc_number BIGINT\
        )",
    // Registre des executions automatisees (#2080). La migration numerotee
    // 039 le monte sur les bases qui suivent la piste numerotee — mais une
    // base creee par l'assistant SQLite -> PostgreSQL porte
    // `schema_version = 99` et ne rejoue JAMAIS les scripts numerotes. Sans
    // cette entree, ces bases-la n'auraient pas la table, la route
    // `/system/task-runs` rendrait une erreur SQL et chaque passe cablee
    // ecrirait dans le vide. C'est exactement la derive documentee sur
    // `lyrics_cache` dans `PG_FULL_SCHEMA`.
    "CREATE TABLE IF NOT EXISTS task_runs (\
            boot_id TEXT NOT NULL,\
            task TEXT NOT NULL,\
            seq BIGINT NOT NULL,\
            started_at TEXT NOT NULL,\
            finished_at TEXT,\
            duration_ms BIGINT,\
            outcome TEXT NOT NULL,\
            items BIGINT,\
            detail TEXT,\
            PRIMARY KEY (boot_id, task, seq)\
        )",
    "CREATE INDEX IF NOT EXISTS idx_task_runs_task_started ON task_runs(task, started_at)",
    "CREATE INDEX IF NOT EXISTS idx_task_runs_outcome ON task_runs(outcome)",
];

// Every column SQLite gains via `add_column_if_missing` that the
// hand-written PG schema omits. All idempotent (ADD COLUMN IF NOT
// EXISTS) and TEXT-typed to match this codebase's TEXT-based PG schema.
// `days_of_week` was the one that surfaced (.15 prod alarm scheduler
// failing every 30s); the others (zones.dsd_mode, is_hidden, …) are
// latent landmines audited from migrations.rs.
//
// Same rule as ENSURE_TABLES: one statement per entry, one round-trip
// each, so a column that cannot be added never hides the ones after it.
pub(crate) const ENSURE_COLUMNS: &[&str] = &[
    "ALTER TABLE alarms ADD COLUMN IF NOT EXISTS days_of_week TEXT DEFAULT '1111111'",
    "ALTER TABLE alarms ADD COLUMN IF NOT EXISTS multi_zone_ids TEXT",
    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS is_hidden TEXT DEFAULT '0'",
    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsd_mode TEXT DEFAULT 'auto'",
    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS autoplay_enabled TEXT DEFAULT '0'",
    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS last_play_state TEXT DEFAULT 'stopped'",
    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS host TEXT",
    "ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS source_id TEXT",
    // BIGINT, pas TEXT : `albums.id` est BIGINT, et la jointure de « Continuer
    // l'ecoute » compare les deux. En TEXT, PostgreSQL rend `operator does not
    // exist: text = bigint` et la section disparait en silence (#2860). Sur une
    // base existante ou la colonne est deja TEXT, cet ADD est un no-op et c'est
    // la migration 047 qui la convertit.
    "ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS album_id BIGINT",
    "ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS profile_id TEXT",
    "ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_type TEXT",
    "ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_id TEXT",
    "ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source TEXT",
    "ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source_url TEXT",
    "ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_license TEXT",
    "ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_lang TEXT",
    "ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT",
    "ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source TEXT",
    "ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source_url TEXT",
    "ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_license TEXT",
    "ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_lang TEXT",
    "ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT",
    "ALTER TABLE playlists ADD COLUMN IF NOT EXISTS profile_id TEXT NOT NULL DEFAULT '1'",
    // #1706: heals a queue_items that predates the numbering columns —
    // the CREATE above only fires on a database that has no queue_items
    // at all. BIGINT (not TEXT): the values are bound as i64 and read
    // back with `as_i64`. Migration 026 converts a column that already
    // drifted to TEXT (SQLite→PG migrated databases).
    "ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS track_number BIGINT",
    "ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS disc_number BIGINT",
];

#[derive(Clone)]
pub struct PostgresDb {
    pool: PgPool,
}

impl PostgresDb {
    /// Connect to PostgreSQL.
    ///
    /// `connection_string` is a libpq DSN, e.g.
    /// `postgresql://user:pass@host:5432/dbname`.
    pub async fn connect(connection_string: &str) -> Result<Self, String> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(5))
            .connect(connection_string)
            .await
            .map_err(|e| format!("postgres connect: {e}"))?;

        info!("postgres_connected");
        let db = Self { pool };
        db.ensure_schema().await;
        Ok(db)
    }

    /// Idempotently add columns that SQLite gains via `add_column_if_missing`
    /// (a SQLite-only helper) but the hand-written PG schema never received, so
    /// an existing Postgres database self-heals on startup instead of erroring
    /// at runtime. `days_of_week`/`multi_zone_ids` were missing on .15 prod →
    /// the alarm scheduler failed every 30s with `column ... does not exist`.
    async fn ensure_schema(&self) {
        self.run_each("pg_ensure_tables_failed", ENSURE_TABLES)
            .await;
        self.run_each("pg_ensure_schema_failed", ENSURE_COLUMNS)
            .await;
    }

    /// Execute self-healing DDL one statement at a time.
    ///
    /// Deliberately NOT a single `raw_sql` batch: PostgreSQL executes a
    /// multi-statement simple query inside one implicit transaction, so the
    /// first failure discards everything — the whole point of #1706. One
    /// round-trip per statement costs a few milliseconds once per boot and
    /// buys the guarantee that a broken table can never mask the next one.
    async fn run_each(&self, event: &'static str, statements: &[&'static str]) {
        for &sql in statements {
            if let Err(e) = sqlx::raw_sql(sql).execute(&self.pool).await {
                // The statement itself is the useful part of the log: it names
                // the table/column that could not be healed.
                warn!(error = %e, statement = %sql, "{}", event);
            }
        }
    }

    /// Smoke-test the pool: runs `SELECT 1`.
    pub async fn ping(&self) -> Result<(), String> {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("postgres ping: {e}"))?;
        Ok(())
    }

    /// Returns the server version string (e.g. `PostgreSQL 16.2 ...`).
    pub async fn server_version(&self) -> Result<String, String> {
        sqlx::query_scalar::<_, String>("SELECT version()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("postgres version: {e}"))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn dialect(&self) -> PostgresDialect {
        PostgresDialect
    }

    pub fn engine(&self) -> Engine {
        Engine::Postgres
    }
}

#[cfg(test)]
mod tests {
    use super::{ENSURE_COLUMNS, ENSURE_TABLES};

    /// The self-healing DDL must stay ONE statement per entry: `run_each`
    /// executes each entry separately precisely so a failure cannot roll the
    /// others back (#1706). A `;`-joined entry would silently restore the old
    /// all-or-nothing batch. `;` inside a `DO $tag$ … $tag$` body is legitimate,
    /// so those entries are checked on the block boundary instead.
    #[test]
    fn ensure_ddl_is_one_statement_per_entry() {
        for sql in ENSURE_TABLES.iter().chain(ENSURE_COLUMNS.iter()) {
            assert!(!sql.trim().is_empty(), "empty DDL entry");
            assert!(
                !sql.trim_end().ends_with(';'),
                "trailing ; makes this a batch again: {sql}"
            );
            if sql.starts_with("DO $") {
                assert_eq!(
                    sql.matches("$ensure$").count(),
                    2,
                    "unbalanced DO block: {sql}"
                );
            } else {
                assert!(
                    !sql.contains(';'),
                    "two statements in one entry defeats run_each: {sql}"
                );
            }
        }
    }

    /// #1706: `queue_items` is created here on any database that predates the
    /// unified queue, and `insert_streaming`/`select_streaming` name
    /// track_number/disc_number. Missing them = no queue write, no queue
    /// restored at boot. Both the CREATE and the ADD COLUMN catch-up must
    /// carry them.
    #[test]
    fn queue_items_ddl_carries_the_numbering_columns() {
        let create = ENSURE_TABLES
            .iter()
            .find(|s| s.contains("CREATE TABLE IF NOT EXISTS queue_items"))
            .expect("queue_items CREATE missing from ENSURE_TABLES");
        assert!(create.contains("track_number BIGINT"), "{create}");
        assert!(create.contains("disc_number BIGINT"), "{create}");

        for col in ["track_number", "disc_number"] {
            let alter = format!("ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS {col} BIGINT");
            assert!(
                ENSURE_COLUMNS.contains(&alter.as_str()),
                "missing catch-up for existing queue_items: {alter}"
            );
        }
    }

    /// The `::text` default is only valid while `streaming_favorites.id` is
    /// still TEXT. Forcing it unconditionally is what aborted the whole batch
    /// on .15 (id is BIGINT there since migration 012).
    #[test]
    fn streaming_favorites_default_is_guarded_on_column_type() {
        let stmt = ENSURE_TABLES
            .iter()
            .find(|s| s.contains("ALTER COLUMN id SET DEFAULT"))
            .expect("streaming_favorites id DEFAULT statement missing");
        assert!(stmt.starts_with("DO $"), "unguarded ALTER: {stmt}");
        assert!(stmt.contains("information_schema.columns"), "{stmt}");
        assert!(stmt.contains("'text', 'character varying'"), "{stmt}");
    }
}
