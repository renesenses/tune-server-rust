//! PostgreSQL backend (phase 2 of the PG support roadmap).
//!
//! Gated by the `postgres` feature flag. This file holds the connection
//! pool primitives — repo migration follows in later phases.
//!
//! See docs/POSTGRES-PLAN.md.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tracing::info;

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
        Ok(Self { pool })
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
