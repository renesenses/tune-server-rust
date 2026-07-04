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
        if let Err(e) = sqlx::query(
            "ALTER TABLE alarms ADD COLUMN IF NOT EXISTS days_of_week TEXT DEFAULT '1111111'",
        )
        .execute(&self.pool)
        .await
        {
            warn!(error = %e, "pg_ensure_schema_days_of_week_failed");
        }
        if let Err(e) =
            sqlx::query("ALTER TABLE alarms ADD COLUMN IF NOT EXISTS multi_zone_ids TEXT")
                .execute(&self.pool)
                .await
        {
            warn!(error = %e, "pg_ensure_schema_multi_zone_ids_failed");
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
