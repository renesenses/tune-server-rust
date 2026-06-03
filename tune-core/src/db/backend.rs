//! `DbBackend` trait — runtime polymorphism between SQLite and Postgres.
//!
//! Phase 5 of the PostgreSQL roadmap. This file lands the trait
//! definition + the SQLite implementation. Postgres impl + the repo
//! refactor follow in subsequent commits.
//!
//! Current state:
//! - `SqliteDb` already exposes `engine()` and `dialect()` so the
//!   13 repos can build engine-agnostic SQL.
//! - Repos still take `SqliteDb` concretely. Phase 5 makes them take
//!   `Arc<dyn DbBackend>` instead, so the same repo code can run on
//!   either backend.
//!
//! Surface chosen:
//! - Keep it minimal — only the methods the repos actually call today
//!   (`read`, `write`, `execute`, `connection`, `read_connection`,
//!   `last_insert_rowid`, `dialect`, `engine`).
//! - The closure-based `read`/`write` shape forces both engines to
//!   wrap their connection abstraction in a synchronous closure boundary
//!   — async surface is layered on top in tune-server as needed.
//! - `last_insert_rowid` keeps SQLite semantics; Postgres returns the
//!   value of `RETURNING id` from the most recent INSERT (managed by
//!   the impl).
//!
//! See `docs/POSTGRES-PLAN.md` for the full phase-5 plan.

use crate::db::engine::Engine;

/// A handle that can execute SQL against either SQLite or Postgres.
/// Repos take `Arc<dyn DbBackend>` and route through the dialect for
/// SQL fragment construction.
///
/// This trait is intentionally non-generic over the SQL execution
/// type — the closure-based `read`/`write` shape avoids leaking
/// rusqlite or sqlx types across the trait boundary. Each impl
/// adapts its native API to that shape.
pub trait DbBackend: Send + Sync {
    /// The engine type for SQL dialect dispatch.
    fn engine(&self) -> Engine;

    /// Execute a statement that doesn't return rows (INSERT/UPDATE/
    /// DELETE/DDL). Returns the number of affected rows.
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String>;

    /// ID of the last row inserted via this backend. Repos that need
    /// the new PK typically call `execute()` then `last_insert_rowid()`.
    /// On Postgres, the impl manages this via `RETURNING id` under the
    /// hood and exposes the latest value here.
    fn last_insert_rowid(&self) -> i64;
}

/// A trait-object-safe wrapper for SQL parameter values. Implemented
/// for the primitives that the repos actually use (`i64`, `f64`,
/// `&str`, `Option<…>`, `bool`).
///
/// Required because we can't put `&dyn rusqlite::types::ToSql` on the
/// `DbBackend` trait (it would leak rusqlite into the public API).
pub trait ToSqlValue: Send + Sync {
    fn to_sql_value(&self) -> SqlValue;
}

/// Type-erased SQL parameter value. The backend impls translate this
/// to their native parameter type at execute time.
#[derive(Debug, Clone)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

// ─── ToSqlValue impls for common types ──────────────────────────────

macro_rules! impl_int {
    ($($t:ty),*) => {
        $(
            impl ToSqlValue for $t {
                fn to_sql_value(&self) -> SqlValue {
                    SqlValue::Int(*self as i64)
                }
            }
        )*
    };
}
impl_int!(i8, i16, i32, i64, u8, u16, u32);

impl ToSqlValue for f32 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Real(*self as f64)
    }
}
impl ToSqlValue for f64 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Real(*self)
    }
}
impl ToSqlValue for bool {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Bool(*self)
    }
}
impl ToSqlValue for str {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Text(self.to_string())
    }
}
impl ToSqlValue for String {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Text(self.clone())
    }
}
impl<T: ToSqlValue> ToSqlValue for Option<T> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => v.to_sql_value(),
            None => SqlValue::Null,
        }
    }
}
impl ToSqlValue for &str {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Text((*self).to_string())
    }
}
impl ToSqlValue for SqlValue {
    fn to_sql_value(&self) -> SqlValue {
        self.clone()
    }
}

// ─── SQLite bridging ──────────────────────────────────────────────────

/// SqlValue → rusqlite::types::Value translation, so SqliteBackend can
/// hand the parameters to rusqlite::Connection::execute().
impl rusqlite::types::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, Value};
        let v = match self {
            SqlValue::Null => Value::Null,
            SqlValue::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
            SqlValue::Int(i) => Value::Integer(*i),
            SqlValue::Real(f) => Value::Real(*f),
            SqlValue::Text(s) => Value::Text(s.clone()),
            SqlValue::Blob(b) => Value::Blob(b.clone()),
        };
        Ok(ToSqlOutput::Owned(v))
    }
}

impl DbBackend for crate::db::sqlite::SqliteDb {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        // Materialize each parameter to an owned SqlValue (cheap — only
        // i64/f64/String/Vec<u8> at worst), then pass references to the
        // ones that implement rusqlite::types::ToSql.
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        self.execute(sql, &refs)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_types_round_trip_via_sql_value() {
        for n in [-128i8, 0, 127] {
            assert!(matches!(n.to_sql_value(), SqlValue::Int(_)));
        }
        for n in [i32::MIN, 0, i32::MAX] {
            assert!(matches!(n.to_sql_value(), SqlValue::Int(_)));
        }
    }

    #[test]
    fn option_some_unwraps_to_inner_type() {
        let v: Option<i64> = Some(42);
        assert!(matches!(v.to_sql_value(), SqlValue::Int(42)));
    }

    #[test]
    fn option_none_maps_to_null() {
        let v: Option<i64> = None;
        assert!(matches!(v.to_sql_value(), SqlValue::Null));
    }

    #[test]
    fn str_and_string_both_map_to_text() {
        assert!(matches!("hello".to_sql_value(), SqlValue::Text(s) if s == "hello"));
        assert!(matches!(String::from("world").to_sql_value(), SqlValue::Text(s) if s == "world"));
    }

    #[test]
    fn bool_maps_to_bool_variant() {
        assert!(matches!(true.to_sql_value(), SqlValue::Bool(true)));
        assert!(matches!(false.to_sql_value(), SqlValue::Bool(false)));
    }

    #[test]
    fn sqlite_backend_round_trip() {
        // End-to-end: hold a SqliteDb behind `Arc<dyn DbBackend>` and
        // execute INSERT through the trait surface, then read back
        // through rusqlite. Proves the bridging layer wires the SqlValue
        // → rusqlite::types::Value translation correctly.
        use std::sync::Arc;

        use super::super::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, score REAL);",
        )
        .unwrap();

        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        assert_eq!(backend.engine(), Engine::Sqlite);

        let name = String::from("miles");
        let score: f64 = 4.5;
        let n = backend
            .execute(
                "INSERT INTO t (name, score) VALUES (?, ?)",
                &[&name, &score],
            )
            .unwrap();
        assert_eq!(n, 1);
        assert!(backend.last_insert_rowid() > 0);

        let opt_str: Option<String> = None;
        let n2 = backend
            .execute("INSERT INTO t (name) VALUES (?)", &[&opt_str])
            .unwrap();
        assert_eq!(n2, 1);

        // Verify the rows landed via direct rusqlite query.
        let conn = db.connection().lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let null_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE name IS NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(null_count, 1);
    }
}
