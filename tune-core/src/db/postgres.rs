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
        // Whole TABLES added to PG_FULL_SCHEMA after a DB was first migrated:
        // PG_FULL_SCHEMA only runs during the one-time SQLite→PG migration, so a
        // table introduced later (e.g. `file_first_seen`, #473) never lands on an
        // already-migrated DB. The albums list sorts "added_at" via a LEFT JOIN
        // on file_first_seen, so its absence made the query fail on .15 prod
        // (`relation "file_first_seen" does not exist`) → empty library → the
        // "black screen" reported on the iOS/Android clients. CREATE TABLE IF NOT
        // EXISTS is idempotent; add new tables here like the columns below.
        const ENSURE_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS file_first_seen (file_path TEXT PRIMARY KEY, first_seen_at DOUBLE PRECISION NOT NULL);";
        if let Err(e) = sqlx::raw_sql(ENSURE_TABLES).execute(&self.pool).await {
            warn!(error = %e, "pg_ensure_tables_failed");
        }

        // Every column SQLite gains via `add_column_if_missing` that the
        // hand-written PG schema omits. All idempotent (ADD COLUMN IF NOT
        // EXISTS) and TEXT-typed to match this codebase's TEXT-based PG schema.
        // `days_of_week` was the one that surfaced (.15 prod alarm scheduler
        // failing every 30s); the others (zones.dsd_mode, is_hidden, …) are
        // latent landmines audited from migrations.rs.
        const ENSURE_COLUMNS: &str = "\
ALTER TABLE alarms ADD COLUMN IF NOT EXISTS days_of_week TEXT DEFAULT '1111111';\
ALTER TABLE alarms ADD COLUMN IF NOT EXISTS multi_zone_ids TEXT;\
ALTER TABLE zones ADD COLUMN IF NOT EXISTS is_hidden TEXT DEFAULT '0';\
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsd_mode TEXT DEFAULT 'auto';\
ALTER TABLE zones ADD COLUMN IF NOT EXISTS autoplay_enabled TEXT DEFAULT '0';\
ALTER TABLE zones ADD COLUMN IF NOT EXISTS last_play_state TEXT DEFAULT 'stopped';\
ALTER TABLE zones ADD COLUMN IF NOT EXISTS host TEXT;\
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS source_id TEXT;\
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS album_id TEXT;\
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS profile_id TEXT;\
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source TEXT;\
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source_url TEXT;\
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_license TEXT;\
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_lang TEXT;\
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT;\
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source TEXT;\
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source_url TEXT;\
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_license TEXT;\
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_lang TEXT;\
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT;";
        if let Err(e) = sqlx::raw_sql(ENSURE_COLUMNS).execute(&self.pool).await {
            warn!(error = %e, "pg_ensure_schema_failed");
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
