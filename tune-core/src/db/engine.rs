//! Database engine abstraction.
//!
//! Phase 1 of the PostgreSQL support roadmap (see docs/POSTGRES-PLAN.md).
//!
//! This module defines the `Engine` enum and the `SqlDialect` trait used
//! by repos to emit engine-specific SQL fragments (placeholders, FTS
//! match clauses, JSON extraction). It is intentionally non-invasive:
//! existing repos continue to use `rusqlite::Connection` directly via
//! `SqliteDb::read` / `SqliteDb::write`; they will opt-in to the dialect
//! helpers as they are migrated repo-by-repo in subsequent phases.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Sqlite,
    Postgres,
}

impl Engine {
    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::Sqlite => "sqlite",
            Engine::Postgres => "postgres",
        }
    }

    /// Parses an engine name. Accepts "sqlite", "postgres", "postgresql".
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sqlite" => Some(Engine::Sqlite),
            "postgres" | "postgresql" => Some(Engine::Postgres),
            _ => None,
        }
    }

    /// Detects the engine from a connection string.
    ///
    /// - `postgresql://...` or `postgres://...` → Postgres
    /// - anything else (including bare paths and `sqlite://`) → SQLite
    pub fn from_connection_string(s: &str) -> Self {
        if s.starts_with("postgresql://") || s.starts_with("postgres://") {
            Engine::Postgres
        } else {
            Engine::Sqlite
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SQL dialect helpers: the small fragments that diverge between SQLite
/// and PostgreSQL. Repos that want to be engine-agnostic build their
/// queries via these helpers.
pub trait SqlDialect {
    fn engine(&self) -> Engine;

    /// Positional placeholder for parameter `idx` (1-based).
    /// SQLite: `?`. Postgres: `$1`, `$2`, ...
    fn placeholder(&self, idx: usize) -> String;

    /// Full-text MATCH clause builder.
    /// SQLite (FTS5): `<column> MATCH <placeholder>`
    /// Postgres (tsvector): `<column> @@ to_tsquery('simple', <placeholder>)`
    fn fts_match(&self, column: &str, placeholder: &str) -> String;

    /// JSON path extraction (returns text).
    /// SQLite: `json_extract(<column>, '<path>')`
    /// Postgres: `<column> #>> '{<path_parts>}'`
    fn json_extract_text(&self, column: &str, path: &str) -> String;

    /// `RETURNING id` for INSERT, when supported by the engine.
    /// SQLite: empty (use `last_insert_rowid` after the INSERT).
    /// Postgres: ` RETURNING id`.
    fn returning_id_clause(&self) -> &'static str;

    /// `ON CONFLICT (...) DO NOTHING` form.
    /// Both engines support it; included so the trait stays the single
    /// source of truth for dialect choices.
    fn on_conflict_do_nothing(&self, conflict_target: &str) -> String {
        format!(" ON CONFLICT ({conflict_target}) DO NOTHING")
    }

    /// `LIMIT ... OFFSET ...` clause. Both engines accept the same form,
    /// but this is the canonical way to opt-in to the dialect helpers.
    fn limit_offset(&self, limit: i64, offset: i64) -> String {
        format!(" LIMIT {limit} OFFSET {offset}")
    }
}

/// Zero-cost dialect for SQLite. Repos hold one of these.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDialect;

impl SqlDialect for SqliteDialect {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn placeholder(&self, _idx: usize) -> String {
        "?".to_string()
    }

    fn fts_match(&self, column: &str, placeholder: &str) -> String {
        format!("{column} MATCH {placeholder}")
    }

    fn json_extract_text(&self, column: &str, path: &str) -> String {
        // Caller is responsible for passing a path that is already
        // single-quote-safe (we don't allow user input here in practice;
        // paths are compile-time string literals).
        format!("json_extract({column}, '{path}')")
    }

    fn returning_id_clause(&self) -> &'static str {
        ""
    }
}

/// Zero-cost dialect for Postgres.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresDialect;

impl SqlDialect for PostgresDialect {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn placeholder(&self, idx: usize) -> String {
        format!("${idx}")
    }

    fn fts_match(&self, column: &str, placeholder: &str) -> String {
        // We use the 'simple' dictionary by default; per-language
        // configuration (french, english, ...) is a follow-up decided in
        // the FTS migration phase.
        format!("{column} @@ to_tsquery('simple', {placeholder})")
    }

    fn json_extract_text(&self, column: &str, path: &str) -> String {
        // Path arrives as a JSON pointer (e.g. "foo.bar") and is
        // translated to a Postgres path array literal.
        let parts: Vec<&str> = path.split('.').collect();
        let path_array = parts.join(",");
        format!("{column} #>> '{{{path_array}}}'")
    }

    fn returning_id_clause(&self) -> &'static str {
        " RETURNING id"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_from_str_accepts_aliases() {
        assert_eq!(Engine::from_str("sqlite"), Some(Engine::Sqlite));
        assert_eq!(Engine::from_str("SQLITE"), Some(Engine::Sqlite));
        assert_eq!(Engine::from_str("postgres"), Some(Engine::Postgres));
        assert_eq!(Engine::from_str("postgresql"), Some(Engine::Postgres));
        assert_eq!(Engine::from_str("mysql"), None);
    }

    #[test]
    fn engine_from_connection_string_routes_correctly() {
        assert_eq!(
            Engine::from_connection_string("/var/lib/tune.db"),
            Engine::Sqlite
        );
        assert_eq!(
            Engine::from_connection_string("postgresql://localhost/tune"),
            Engine::Postgres
        );
        assert_eq!(
            Engine::from_connection_string("postgres://u:p@host/db"),
            Engine::Postgres
        );
    }

    #[test]
    fn sqlite_dialect_emits_question_marks() {
        let d = SqliteDialect;
        assert_eq!(d.placeholder(1), "?");
        assert_eq!(d.placeholder(42), "?");
        assert_eq!(
            d.fts_match("tracks_fts", d.placeholder(1).as_str()),
            "tracks_fts MATCH ?"
        );
        assert_eq!(d.returning_id_clause(), "");
    }

    #[test]
    fn postgres_dialect_emits_numbered_placeholders() {
        let d = PostgresDialect;
        assert_eq!(d.placeholder(1), "$1");
        assert_eq!(d.placeholder(7), "$7");
        assert_eq!(
            d.fts_match("search_tsv", d.placeholder(1).as_str()),
            "search_tsv @@ to_tsquery('simple', $1)"
        );
        assert_eq!(d.returning_id_clause(), " RETURNING id");
    }

    #[test]
    fn json_extract_dialect_specific() {
        let s = SqliteDialect;
        assert_eq!(
            s.json_extract_text("meta", "artist.name"),
            "json_extract(meta, 'artist.name')"
        );
        let p = PostgresDialect;
        assert_eq!(
            p.json_extract_text("meta", "artist.name"),
            "meta #>> '{artist,name}'"
        );
    }

    #[test]
    fn engine_display_matches_as_str() {
        assert_eq!(format!("{}", Engine::Sqlite), "sqlite");
        assert_eq!(format!("{}", Engine::Postgres), "postgres");
    }
}
