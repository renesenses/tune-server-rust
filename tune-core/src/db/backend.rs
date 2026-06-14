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

    /// Read at most one row. Returns the row's columns as `SqlValue`s
    /// in declaration order. The repo decodes them via the `as_i64` /
    /// `as_str` / `as_f64` helpers on `SqlValue`.
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String>;

    /// Read all matching rows. Same row representation as `query_one`.
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String>;

    /// Run a sequence of writes inside a transaction. The closure
    /// receives a handle that has the same trait surface as
    /// `DbBackend` for `execute` / `query_one` / `query_many` /
    /// `last_insert_rowid`. The impl commits on `Ok` return and rolls
    /// back on `Err` or panic.
    ///
    /// The repo that needs to atomically read-then-write (e.g.
    /// `playlist_repo::add_tracks`) calls this instead of taking the
    /// connection lock directly.
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String>;

    /// Same as `query_many`, but reads through the write path so the
    /// query sees commits made by the write connection that haven't
    /// yet propagated to the read snapshot.
    ///
    /// SQLite + WAL: the read-only connection takes a snapshot at open
    /// time. During a long-running write tx, that snapshot can lag —
    /// `list()` may return 0 rows for a row that was just committed by
    /// the write conn. Calling `query_many_strong` instead reads via
    /// the write conn, which always sees its own commits. This is the
    /// pattern from commit `8af95ec` (play_queue) and the `zone_repo`
    /// P0 fix for forum bugs #2 and #6.
    ///
    /// On Postgres, the same `PgPool` is used regardless, so this
    /// method is functionally identical to `query_many` — the call
    /// site documents its own intent.
    ///
    /// Default impl delegates to `query_many`; SQLite overrides.
    fn query_many_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.query_many(sql, params)
    }

    /// Like `query_one`, but reads through the write connection so the
    /// query sees rows inserted in the current write transaction.
    ///
    /// Same WAL snapshot rationale as `query_many_strong`: the read-only
    /// connection pool may not see rows that the write connection
    /// inserted inside a `BEGIN IMMEDIATE` that has not yet been
    /// committed. This is the root cause of the album-splitting bug
    /// where each track in a batch scan created its own album — the
    /// `SELECT` to check for an existing album ran on a read connection
    /// that couldn't see the album just created by the write connection.
    ///
    /// Default impl delegates to `query_one`; SQLite overrides.
    fn query_one_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.query_one(sql, params)
    }
}

/// Transaction handle. Mirror of `DbBackend`'s execution surface, but
/// scoped to a single transaction. Live only inside the
/// `DbBackend::write_tx` closure — the trait object goes out of scope
/// as soon as the closure returns.
pub trait DbTxHandle {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String>;
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String>;
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String>;
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
///
/// The typed `Null*` variants exist because PostgreSQL is strict
/// about parameter types — sending `NULL` as TEXT for a `BIGINT`
/// column raises a parse-time error. SQLite ignores the type tag.
#[derive(Debug, Clone)]
pub enum SqlValue {
    /// NULL with unspecified type. Defaults to BIGINT NULL on PG.
    Null,
    /// NULL targeting a BIGINT / INT4 / INT2 column.
    NullInt,
    /// NULL targeting a TEXT / VARCHAR column.
    NullText,
    /// NULL targeting a DOUBLE PRECISION / REAL column.
    NullReal,
    /// NULL targeting a BOOLEAN column.
    NullBool,
    /// NULL targeting a BYTEA column.
    NullBlob,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// Returns true if this is any SQL `NULL` variant.
    pub fn is_null(&self) -> bool {
        matches!(
            self,
            SqlValue::Null
                | SqlValue::NullInt
                | SqlValue::NullText
                | SqlValue::NullReal
                | SqlValue::NullBool
                | SqlValue::NullBlob
        )
    }

    /// Decode as `i64`. Returns `None` for any NULL variant; coerces
    /// `Bool` to 0/1, `Real` to truncated int, `Text` via parse.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Int(i) => Some(*i),
            SqlValue::Bool(b) => Some(if *b { 1 } else { 0 }),
            SqlValue::Real(f) => Some(*f as i64),
            SqlValue::Text(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Decode as `f64`. Returns `None` for NULL variants.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Real(f) => Some(*f),
            SqlValue::Int(i) => Some(*i as f64),
            SqlValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            SqlValue::Text(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Decode as `bool`. SQLite stores bools as INT; we accept either.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SqlValue::Bool(b) => Some(*b),
            SqlValue::Int(i) => Some(*i != 0),
            _ => None,
        }
    }

    /// Decode as `&str`. Returns `None` for NULL variants.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SqlValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Decode as owned `String`. Returns `None` for NULL variants.
    pub fn as_string(&self) -> Option<String> {
        match self {
            SqlValue::Text(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Decode as `&[u8]`. Returns `None` for NULL variants.
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            SqlValue::Blob(b) => Some(b.as_slice()),
            _ => None,
        }
    }
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
// Specific Option<T> impls so `None` carries a type hint to PG via
// the typed `SqlValue::Null*` variants. SQLite ignores the variant.
macro_rules! impl_option_int {
    ($($t:ty),*) => {
        $(
            impl ToSqlValue for Option<$t> {
                fn to_sql_value(&self) -> SqlValue {
                    match self {
                        Some(v) => SqlValue::Int(*v as i64),
                        None => SqlValue::NullInt,
                    }
                }
            }
        )*
    };
}
impl_option_int!(i8, i16, i32, i64, u8, u16, u32);

impl ToSqlValue for Option<f32> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Real(*v as f64),
            None => SqlValue::NullReal,
        }
    }
}
impl ToSqlValue for Option<f64> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Real(*v),
            None => SqlValue::NullReal,
        }
    }
}
impl ToSqlValue for Option<bool> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Bool(*v),
            None => SqlValue::NullBool,
        }
    }
}
impl ToSqlValue for Option<String> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Text(v.clone()),
            None => SqlValue::NullText,
        }
    }
}
impl ToSqlValue for Option<&str> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Text((*v).to_string()),
            None => SqlValue::NullText,
        }
    }
}
impl ToSqlValue for Option<Vec<u8>> {
    fn to_sql_value(&self) -> SqlValue {
        match self {
            Some(v) => SqlValue::Blob(v.clone()),
            None => SqlValue::NullBlob,
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
/// hand the parameters to rusqlite::Connection::execute(). SQLite is
/// dynamically typed so all `Null*` variants collapse to `Value::Null`.
impl rusqlite::types::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, Value};
        let v = match self {
            SqlValue::Null
            | SqlValue::NullInt
            | SqlValue::NullText
            | SqlValue::NullReal
            | SqlValue::NullBool
            | SqlValue::NullBlob => Value::Null,
            SqlValue::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
            SqlValue::Int(i) => Value::Integer(*i),
            SqlValue::Real(f) => Value::Real(*f),
            SqlValue::Text(s) => Value::Text(s.clone()),
            SqlValue::Blob(b) => Value::Blob(b.clone()),
        };
        Ok(ToSqlOutput::Owned(v))
    }
}

/// Translate a single rusqlite column to `SqlValue` using its runtime type.
fn rusqlite_value_to_sqlvalue(v: rusqlite::types::ValueRef<'_>) -> SqlValue {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Int(i),
        ValueRef::Real(f) => SqlValue::Real(f),
        ValueRef::Text(b) => SqlValue::Text(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
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

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let conn = self.read_connection().lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("query: {e}"))?;
        if let Some(row) = rows.next().map_err(|e| format!("row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(cols))
        } else {
            Ok(None)
        }
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let conn = self.read_connection().lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("query: {e}"))?;
        let mut out: Vec<Vec<SqlValue>> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| format!("row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.push(cols);
        }
        Ok(out)
    }

    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut conn = self.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
        let result = {
            let handle = SqliteTxHandle { tx: &tx };
            f(&handle)
        };
        match result {
            Ok(()) => tx.commit().map_err(|e| format!("commit: {e}")),
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }

    fn query_many_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        // Same body as query_many but uses the *write* connection so
        // the query sees the writer's own commits — circumvents the
        // WAL snapshot lag on the read-only conn. See the trait
        // docstring + commit 8af95ec for the motivating bug.
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let conn = self.connection().lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("query: {e}"))?;
        let mut out: Vec<Vec<SqlValue>> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| format!("row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.push(cols);
        }
        Ok(out)
    }

    fn query_one_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        // Same body as query_one but uses the *write* connection so the
        // query sees rows inserted in the current write transaction.
        // See query_many_strong and the album-splitting bug.
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let conn = self.connection().lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("query: {e}"))?;
        if let Some(row) = rows.next().map_err(|e| format!("row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(cols))
        } else {
            Ok(None)
        }
    }
}

/// SQLite-backed transaction handle. Wraps a `rusqlite::Transaction`
/// for the duration of the `write_tx` closure.
pub struct SqliteTxHandle<'a> {
    tx: &'a rusqlite::Transaction<'a>,
}

impl DbTxHandle for SqliteTxHandle<'_> {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        self.tx
            .execute(sql, refs.as_slice())
            .map_err(|e| format!("tx execute: {e}"))
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = self
            .tx
            .prepare(sql)
            .map_err(|e| format!("tx prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("tx query: {e}"))?;
        if let Some(row) = rows.next().map_err(|e| format!("tx row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("tx col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(cols))
        } else {
            Ok(None)
        }
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = self
            .tx
            .prepare(sql)
            .map_err(|e| format!("tx prepare: {e}"))?;
        let col_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|e| format!("tx query: {e}"))?;
        let mut out: Vec<Vec<SqlValue>> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| format!("tx row: {e}"))? {
            let cols = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(rusqlite_value_to_sqlvalue)
                        .map_err(|e| format!("tx col {i}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.push(cols);
        }
        Ok(out)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.tx.last_insert_rowid()
    }
}

// ─── Postgres bridging ────────────────────────────────────────────────

/// PostgresBackend: sqlx::PgPool wrapped to fit the DbBackend trait.
///
/// The trait is synchronous but sqlx is async; the impl bridges via
/// `tokio::task::block_in_place` + `Handle::current().block_on(...)`.
/// That requires a multi-threaded tokio runtime — Tune uses
/// `#[tokio::main]` which provides one by default.
///
/// `last_insert_rowid`: Postgres has no equivalent of SQLite's
/// last_insert_rowid(), but every Tune INSERT that needs the new PK
/// goes through `RETURNING id` (added by `dialect.returning_id_clause`).
/// The impl detects `RETURNING id` in the SQL and routes through
/// `fetch_one` to capture the id into an internal mutex, exposed via
/// `last_insert_rowid()`.
#[cfg(feature = "postgres")]
pub struct PostgresBackend {
    pool: sqlx::PgPool,
    last_id: std::sync::Arc<std::sync::Mutex<i64>>,
}

#[cfg(feature = "postgres")]
impl PostgresBackend {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            last_id: std::sync::Arc::new(std::sync::Mutex::new(0)),
        }
    }
}

#[cfg(feature = "postgres")]
fn bind_sqlvalue<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &SqlValue,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match v {
        // Unspecified NULL defaults to BIGINT (the most common FK
        // shape in our schema). The typed variants below pin the
        // PG-side parameter type to match the column.
        SqlValue::Null | SqlValue::NullInt => q.bind::<Option<i64>>(None),
        SqlValue::NullText => q.bind::<Option<String>>(None),
        SqlValue::NullReal => q.bind::<Option<f64>>(None),
        SqlValue::NullBool => q.bind::<Option<bool>>(None),
        SqlValue::NullBlob => q.bind::<Option<Vec<u8>>>(None),
        SqlValue::Bool(b) => q.bind(*b),
        SqlValue::Int(i) => q.bind(*i),
        SqlValue::Real(f) => q.bind(*f),
        SqlValue::Text(s) => q.bind(s.clone()),
        SqlValue::Blob(b) => q.bind(b.clone()),
    }
}

#[cfg(feature = "postgres")]
fn bind_sqlvalue_scalar<'q>(
    q: sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    v: &SqlValue,
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments> {
    match v {
        SqlValue::Null | SqlValue::NullInt => q.bind::<Option<i64>>(None),
        SqlValue::NullText => q.bind::<Option<String>>(None),
        SqlValue::NullReal => q.bind::<Option<f64>>(None),
        SqlValue::NullBool => q.bind::<Option<bool>>(None),
        SqlValue::NullBlob => q.bind::<Option<Vec<u8>>>(None),
        SqlValue::Bool(b) => q.bind(*b),
        SqlValue::Int(i) => q.bind(*i),
        SqlValue::Real(f) => q.bind(*f),
        SqlValue::Text(s) => q.bind(s.clone()),
        SqlValue::Blob(b) => q.bind(b.clone()),
    }
}

#[cfg(feature = "postgres")]
impl DbBackend for PostgresBackend {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let pool = self.pool.clone();
        let last_id_handle = self.last_id.clone();
        let mut sql_owned = sql.to_string();
        // Auto-append `RETURNING id` to bare INSERT statements so
        // `last_insert_rowid()` works on PG (it has no equivalent of
        // SQLite's per-connection last-insert id). Detection is
        // syntactic: case-insensitive INSERT INTO prefix, no existing
        // RETURNING clause. Statements that already specify RETURNING
        // (or that aren't INSERTs) pass through unchanged.
        let upper = sql_owned.to_ascii_uppercase();
        let trimmed = upper.trim_end_matches(';').trim_end();
        let is_insert = trimmed.trim_start().starts_with("INSERT INTO");
        let has_returning = trimmed.contains("RETURNING");
        if is_insert && !has_returning {
            let trailing_semi = sql_owned.ends_with(';');
            // Strip trailing whitespace + ';' before adding the clause.
            sql_owned = sql_owned.trim_end().trim_end_matches(';').to_string();
            sql_owned.push_str(" RETURNING id");
            if trailing_semi {
                sql_owned.push(';');
            }
        }
        let returning = is_insert || trimmed.ends_with("RETURNING ID");

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                if returning {
                    let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql_owned));
                    for v in &owned {
                        q = bind_sqlvalue_scalar(q, v);
                    }
                    let id = q
                        .fetch_one(&pool)
                        .await
                        .map_err(|e| format!("pg execute (returning): {e}"))?;
                    *last_id_handle.lock().unwrap() = id;
                    Ok(1)
                } else {
                    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                    for v in &owned {
                        q = bind_sqlvalue(q, v);
                    }
                    let result = q
                        .execute(&pool)
                        .await
                        .map_err(|e| format!("pg execute: {e}"))?;
                    Ok(result.rows_affected() as usize)
                }
            })
        })
    }

    fn last_insert_rowid(&self) -> i64 {
        *self.last_id.lock().unwrap()
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let pool = self.pool.clone();
        let sql_owned = sql.to_string();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                for v in &owned {
                    q = bind_sqlvalue(q, v);
                }
                let row_opt = q
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| format!("pg query_one: {e}"))?;
                match row_opt {
                    None => Ok(None),
                    Some(row) => Ok(Some(pgrow_to_sqlvalues(&row)?)),
                }
            })
        })
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let pool = self.pool.clone();
        let sql_owned = sql.to_string();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                for v in &owned {
                    q = bind_sqlvalue(q, v);
                }
                let rows = q
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| format!("pg query_many: {e}"))?;
                rows.iter().map(pgrow_to_sqlvalues).collect()
            })
        })
    }

    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        let pool = self.pool.clone();
        let last_id_handle = self.last_id.clone();

        // Bridge async sqlx transaction into the sync closure boundary.
        // `block_in_place` is safe here because Tune runs on a
        // multi-threaded tokio runtime (`#[tokio::main]`).
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("pg begin tx: {e}"))?;

                let tx_cell = std::cell::RefCell::new(tx);
                let handle = PgTxHandle {
                    tx: &tx_cell,
                    last_id: last_id_handle.clone(),
                };

                let result = f(&handle);

                let tx = tx_cell.into_inner();
                match result {
                    Ok(()) => {
                        tx.commit().await.map_err(|e| format!("pg commit: {e}"))?;
                        Ok(())
                    }
                    Err(e) => {
                        tx.rollback()
                            .await
                            .map_err(|e2| format!("pg rollback after error ({e}): {e2}"))?;
                        Err(e)
                    }
                }
            })
        })
    }
}

/// Postgres-backed transaction handle. Wraps a `RefCell`-guarded
/// reference to a `sqlx::Transaction` for the duration of the
/// `write_tx` closure.
///
/// Each operation inside the closure runs via `block_in_place` +
/// `block_on` to bridge async sqlx into the sync `DbTxHandle` trait.
/// The `RefCell` provides interior mutability so the `&self` methods
/// on `DbTxHandle` can borrow the transaction mutably (sqlx requires
/// `&mut Transaction` for execute/fetch). Single-threaded access is
/// guaranteed because `write_tx` is synchronous and the handle never
/// escapes the closure.
#[cfg(feature = "postgres")]
struct PgTxHandle<'a> {
    tx: &'a std::cell::RefCell<sqlx::Transaction<'static, sqlx::Postgres>>,
    last_id: std::sync::Arc<std::sync::Mutex<i64>>,
}

#[cfg(feature = "postgres")]
impl DbTxHandle for PgTxHandle<'_> {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let mut sql_owned = sql.to_string();
        let last_id_handle = self.last_id.clone();

        // Auto-append RETURNING id for bare INSERTs (same logic as
        // PostgresBackend::execute).
        let upper = sql_owned.to_ascii_uppercase();
        let trimmed = upper.trim_end_matches(';').trim_end();
        let is_insert = trimmed.trim_start().starts_with("INSERT INTO");
        let has_returning = trimmed.contains("RETURNING");
        if is_insert && !has_returning {
            let trailing_semi = sql_owned.ends_with(';');
            sql_owned = sql_owned.trim_end().trim_end_matches(';').to_string();
            sql_owned.push_str(" RETURNING id");
            if trailing_semi {
                sql_owned.push(';');
            }
        }
        let returning = is_insert || trimmed.ends_with("RETURNING ID");

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut tx_guard = self.tx.borrow_mut();
                if returning {
                    let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql_owned));
                    for v in &owned {
                        q = bind_sqlvalue_scalar(q, v);
                    }
                    let id = q
                        .fetch_one(&mut **tx_guard)
                        .await
                        .map_err(|e| format!("pg tx execute (returning): {e}"))?;
                    *last_id_handle.lock().unwrap() = id;
                    Ok(1)
                } else {
                    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                    for v in &owned {
                        q = bind_sqlvalue(q, v);
                    }
                    let result = q
                        .execute(&mut **tx_guard)
                        .await
                        .map_err(|e| format!("pg tx execute: {e}"))?;
                    Ok(result.rows_affected() as usize)
                }
            })
        })
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let sql_owned = sql.to_string();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut tx_guard = self.tx.borrow_mut();
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                for v in &owned {
                    q = bind_sqlvalue(q, v);
                }
                let row_opt = q
                    .fetch_optional(&mut **tx_guard)
                    .await
                    .map_err(|e| format!("pg tx query_one: {e}"))?;
                match row_opt {
                    None => Ok(None),
                    Some(row) => Ok(Some(pgrow_to_sqlvalues(&row)?)),
                }
            })
        })
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        let owned: Vec<SqlValue> = params.iter().map(|p| p.to_sql_value()).collect();
        let sql_owned = sql.to_string();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut tx_guard = self.tx.borrow_mut();
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_owned));
                for v in &owned {
                    q = bind_sqlvalue(q, v);
                }
                let rows = q
                    .fetch_all(&mut **tx_guard)
                    .await
                    .map_err(|e| format!("pg tx query_many: {e}"))?;
                rows.iter().map(pgrow_to_sqlvalues).collect()
            })
        })
    }

    fn last_insert_rowid(&self) -> i64 {
        *self.last_id.lock().unwrap()
    }
}

#[cfg(feature = "postgres")]
fn pgrow_to_sqlvalues(row: &sqlx::postgres::PgRow) -> Result<Vec<SqlValue>, String> {
    use sqlx::{Column, Row, TypeInfo};
    let n = row.columns().len();
    let mut out: Vec<SqlValue> = Vec::with_capacity(n);
    for i in 0..n {
        let col = &row.columns()[i];
        let type_name = col.type_info().name();
        // Try NULL first via try_get::<Option<T>, _>. Dispatch on type name
        // to pick the right Rust type. List covers the types Tune actually
        // stores (cf. migrations/postgres/001_initial_schema.sql).
        let v = match type_name {
            "INT2" | "SMALLINT" => row
                .try_get::<Option<i16>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, |v| SqlValue::Int(v as i64)))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "INT4" | "INTEGER" | "INT" => row
                .try_get::<Option<i32>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, |v| SqlValue::Int(v as i64)))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "INT8" | "BIGINT" => row
                .try_get::<Option<i64>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, SqlValue::Int))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "FLOAT4" | "REAL" => row
                .try_get::<Option<f32>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, |v| SqlValue::Real(v as f64)))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "FLOAT8" | "DOUBLE PRECISION" => row
                .try_get::<Option<f64>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, SqlValue::Real))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "BOOL" => row
                .try_get::<Option<bool>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, SqlValue::Bool))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            "BYTEA" => row
                .try_get::<Option<Vec<u8>>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, SqlValue::Blob))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
            // TEXT, VARCHAR, BPCHAR, CITEXT, NAME, JSON, JSONB, UUID, TIMESTAMP*, DATE
            // all decode cleanly via try_get::<Option<String>, _>().
            _ => row
                .try_get::<Option<String>, _>(i)
                .map(|o| o.map_or(SqlValue::Null, SqlValue::Text))
                .map_err(|e| format!("pg col {i} ({type_name}): {e}"))?,
        };
        out.push(v);
    }
    Ok(out)
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
    fn option_none_maps_to_typed_null() {
        let v: Option<i64> = None;
        assert!(matches!(v.to_sql_value(), SqlValue::NullInt));
        let v: Option<String> = None;
        assert!(matches!(v.to_sql_value(), SqlValue::NullText));
        let v: Option<f64> = None;
        assert!(matches!(v.to_sql_value(), SqlValue::NullReal));
        let v: Option<bool> = None;
        assert!(matches!(v.to_sql_value(), SqlValue::NullBool));
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
    fn sqlite_write_tx_commits_on_ok() {
        use std::sync::Arc;

        use super::super::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, score REAL);",
        )
        .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());

        let name = String::from("inside-tx");
        backend
            .write_tx(&mut |tx| {
                let n: &dyn ToSqlValue = &name;
                tx.execute("INSERT INTO t (name) VALUES (?)", &[n])?;
                Ok(())
            })
            .unwrap();

        let conn = db.connection().lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn sqlite_write_tx_rolls_back_on_err() {
        use std::sync::Arc;

        use super::super::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);")
            .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());

        let name = String::from("would-rollback");
        let _ = backend.write_tx(&mut |tx| {
            let n: &dyn ToSqlValue = &name;
            tx.execute("INSERT INTO t (name) VALUES (?)", &[n])?;
            Err("intentional".into())
        });

        let conn = db.connection().lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "rollback should have undone the INSERT");
    }

    #[test]
    fn sqlite_write_tx_last_insert_rowid() {
        use std::sync::Arc;

        use super::super::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);")
            .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let captured = std::cell::Cell::new(0i64);
        backend
            .write_tx(&mut |tx| {
                let n: &dyn ToSqlValue = &"x".to_string();
                tx.execute("INSERT INTO t (name) VALUES (?)", &[n])?;
                captured.set(tx.last_insert_rowid());
                Ok(())
            })
            .unwrap();
        assert!(captured.get() > 0);
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
