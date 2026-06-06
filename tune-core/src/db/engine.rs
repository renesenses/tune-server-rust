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

/// Format a user-supplied search query for the engine's FTS dialect.
///
/// Splits on whitespace, strips non-alphanumeric chars (defensive), and
/// joins per engine:
/// - SQLite FTS5: `term1 term2*` (space-separated, prefix on last)
/// - Postgres tsquery: `term1 & term2:*` (AND, prefix on last)
///
/// Returns an empty string if the input has no usable tokens, so the
/// caller can short-circuit to a LIKE-only path.
pub fn format_fts_query(engine: Engine, raw: &str) -> String {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return String::new();
    }
    let mut owned = tokens;
    let last = owned.pop().expect("non-empty");
    match engine {
        Engine::Sqlite => {
            // FTS5 accepts space-separated tokens with implicit AND.
            let prefix = if owned.is_empty() {
                String::new()
            } else {
                format!("{} ", owned.join(" "))
            };
            format!("{prefix}{last}*")
        }
        Engine::Postgres => {
            // tsquery requires explicit operators between tokens; the
            // prefix marker `:*` only goes on the last token.
            let prefix = if owned.is_empty() {
                String::new()
            } else {
                format!("{} & ", owned.join(" & "))
            };
            format!("{prefix}{last}:*")
        }
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

    /// Full-text MATCH clause builder (low-level column fragment).
    /// SQLite (FTS5): `<column> MATCH <placeholder>`
    /// Postgres (tsvector): `<column> @@ to_tsquery('simple', <placeholder>)`
    fn fts_match(&self, column: &str, placeholder: &str) -> String;

    /// Full-text search WHERE clause for a base table.
    ///
    /// Different engines need fundamentally different shapes here:
    /// SQLite goes through the FTS5 virtual table so the predicate is
    /// `id IN (SELECT rowid FROM <table>_fts WHERE …)`; Postgres has
    /// the tsvector on the base table itself, so the predicate is a
    /// direct `<alias>.search_tsv @@ …`.
    ///
    /// `table_alias` is the alias used in the outer query (`a` for
    /// albums, `t` for tracks, etc.) so the SQLite branch can emit
    /// `<alias>.id IN (...)`.
    ///
    /// `query_placeholder` is the bound parameter for the search
    /// query string (e.g. `$1` or `?`). The caller is responsible for
    /// formatting the user's input so it's valid for both backends:
    /// FTS5 wants `term*`, tsquery wants `term:*`. The repos pass
    /// engine-specific strings in.
    fn fts_where(&self, table: &str, table_alias: &str, query_placeholder: &str) -> String;

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

    /// Current UTC timestamp formatted as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`).
    /// Used by `history_repo` (and friends) for `WHERE listened_at >=
    /// (now - N days)` aggregations.
    ///
    /// SQLite: `strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`
    /// Postgres: `to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')`
    fn now_iso8601(&self) -> &'static str;

    /// SQL fragment computing `<column> >= now - N days`. Used for
    /// rolling-window aggregations in `history_repo::full_dashboard`.
    ///
    /// SQLite: `<column> >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')`
    /// Postgres: `<column> >= to_char(now() - interval '{days} days', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')`
    fn since_days(&self, column: &str, days: i64) -> String;

    /// Date truncation to the day, returned as ISO date `YYYY-MM-DD`.
    ///
    /// SQLite: `DATE(<column>)`
    /// Postgres: `to_char(<column>::timestamp, 'YYYY-MM-DD')`
    fn date_trunc_day(&self, column: &str) -> String;

    /// Build a predicate that's true when a JSON-array text column
    /// contains a value (case-insensitive). Used by
    /// `album_repo::list_by_genre` for the structured `genres` column.
    ///
    /// SQLite: `EXISTS (SELECT 1 FROM json_each(<column>) WHERE LOWER(value) = LOWER(<placeholder>))`
    /// Postgres: `EXISTS (SELECT 1 FROM jsonb_array_elements_text(<column>::jsonb) AS x(v) WHERE LOWER(v) = LOWER(<placeholder>))`
    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String;

    /// Extract the hour (0-23) from a timestamp column as an integer.
    /// Used by `history_repo::full_dashboard` for hourly listening
    /// distribution.
    ///
    /// SQLite: `CAST(strftime('%H', <column>) AS INTEGER)`
    /// Postgres: `EXTRACT(HOUR FROM <column>::timestamp)::int`
    fn extract_hour(&self, column: &str) -> String;

    /// Current UTC timestamp expression, suitable for use as a DEFAULT
    /// or in INSERT VALUES.
    ///
    /// SQLite: `datetime('now')`
    /// Postgres: `NOW()`
    fn current_timestamp_expr(&self) -> &'static str;

    /// Boolean literal (for engines that don't support native booleans).
    ///
    /// SQLite: `1` / `0`
    /// Postgres: `TRUE` / `FALSE`
    fn bool_literal(&self, val: bool) -> &'static str;

    /// GROUP_CONCAT or STRING_AGG for aggregating text values.
    ///
    /// SQLite: `GROUP_CONCAT(<column>, <separator>)`
    /// Postgres: `STRING_AGG(<column>, <separator>)`
    fn group_concat(&self, column: &str, separator: &str) -> String;
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

    fn fts_where(&self, table: &str, table_alias: &str, query_placeholder: &str) -> String {
        format!(
            "{table_alias}.id IN (SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH {query_placeholder})"
        )
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

    fn now_iso8601(&self) -> &'static str {
        "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    }

    fn since_days(&self, column: &str, days: i64) -> String {
        // SQLite's modifier syntax: 'now', '-N days' → relative to now.
        format!("{column} >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')")
    }

    fn date_trunc_day(&self, column: &str) -> String {
        format!("DATE({column})")
    }

    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM json_each({column}) WHERE LOWER(value) = LOWER({placeholder}))"
        )
    }

    fn extract_hour(&self, column: &str) -> String {
        format!("CAST(strftime('%H', {column}) AS INTEGER)")
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "datetime('now')"
    }

    fn bool_literal(&self, val: bool) -> &'static str {
        if val { "1" } else { "0" }
    }

    fn group_concat(&self, column: &str, separator: &str) -> String {
        format!("GROUP_CONCAT({column}, '{separator}')")
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
        format!("{column} @@ to_tsquery('simple', unaccent({placeholder}))")
    }

    fn fts_where(&self, _table: &str, table_alias: &str, query_placeholder: &str) -> String {
        // PG has the tsvector on the base table itself (see
        // tune-core/migrations/postgres/002_fts_tsvector.sql), so the
        // predicate is a direct @@ on <alias>.search_tsv. Wrapping the
        // placeholder in unaccent() makes the search accent-insensitive,
        // matching the behaviour of FTS5's `tokenize='unicode61
        // remove_diacritics 2'`.
        format!("{table_alias}.search_tsv @@ to_tsquery('simple', unaccent({query_placeholder}))")
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

    fn now_iso8601(&self) -> &'static str {
        // The `T` and `Z` literals need to be quoted within to_char's
        // format string.
        "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
    }

    fn since_days(&self, column: &str, days: i64) -> String {
        format!(
            "{column} >= to_char(now() - interval '{days} days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    }

    fn date_trunc_day(&self, column: &str) -> String {
        // `listened_at` is stored as TEXT on both engines (ISO-8601),
        // so cast to timestamp before format.
        format!("to_char({column}::timestamp, 'YYYY-MM-DD')")
    }

    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM jsonb_array_elements_text({column}::jsonb) AS x(v) WHERE LOWER(v) = LOWER({placeholder}))"
        )
    }

    fn extract_hour(&self, column: &str) -> String {
        format!("EXTRACT(HOUR FROM {column}::timestamp)::int")
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    fn bool_literal(&self, val: bool) -> &'static str {
        if val { "TRUE" } else { "FALSE" }
    }

    fn group_concat(&self, column: &str, separator: &str) -> String {
        format!("STRING_AGG({column}, '{separator}')")
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
            "search_tsv @@ to_tsquery('simple', unaccent($1))"
        );
        assert_eq!(d.returning_id_clause(), " RETURNING id");
    }

    #[test]
    fn fts_where_uses_engine_specific_shape() {
        let s = SqliteDialect;
        assert_eq!(
            s.fts_where("artists", "a", &s.placeholder(1)),
            "a.id IN (SELECT rowid FROM artists_fts WHERE artists_fts MATCH ?)"
        );
        let p = PostgresDialect;
        assert_eq!(
            p.fts_where("artists", "a", &p.placeholder(1)),
            "a.search_tsv @@ to_tsquery('simple', unaccent($1))"
        );
    }

    #[test]
    fn format_fts_query_single_word() {
        assert_eq!(format_fts_query(Engine::Sqlite, "miles"), "miles*");
        assert_eq!(format_fts_query(Engine::Postgres, "miles"), "miles:*");
    }

    #[test]
    fn format_fts_query_multi_word() {
        // Multi-word inputs are AND-joined per engine syntax. This is
        // the bug that crashed tsquery in the v0.8.28 PG smoke test.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "miles davis"),
            "miles davis*"
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "miles davis"),
            "miles & davis:*"
        );
    }

    #[test]
    fn format_fts_query_strips_punctuation() {
        assert_eq!(
            format_fts_query(Engine::Postgres, "rock & roll!"),
            "rock & roll:*"
        );
        // The user's `&` is stripped along with other non-alphanumerics
        // before we re-introduce it as the tsquery AND operator.
    }

    #[test]
    fn format_fts_query_empty_returns_empty() {
        assert_eq!(format_fts_query(Engine::Sqlite, ""), "");
        assert_eq!(format_fts_query(Engine::Postgres, "   !!!"), "");
    }

    #[test]
    fn format_fts_query_handles_unicode_letters() {
        // is_alphanumeric() accepts the Stromaé é; that's fine — we
        // rely on PG's unaccent() to handle the diacritics downstream
        // at query time.
        assert_eq!(format_fts_query(Engine::Postgres, "stromaé"), "stromaé:*");
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

    #[test]
    fn current_timestamp_expr_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.current_timestamp_expr(), "datetime('now')");
        let p = PostgresDialect;
        assert_eq!(p.current_timestamp_expr(), "NOW()");
    }

    #[test]
    fn bool_literal_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.bool_literal(true), "1");
        assert_eq!(s.bool_literal(false), "0");
        let p = PostgresDialect;
        assert_eq!(p.bool_literal(true), "TRUE");
        assert_eq!(p.bool_literal(false), "FALSE");
    }

    #[test]
    fn group_concat_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.group_concat("name", ", "), "GROUP_CONCAT(name, ', ')");
        let p = PostgresDialect;
        assert_eq!(p.group_concat("name", ", "), "STRING_AGG(name, ', ')");
    }

    #[test]
    fn date_helpers_dialect_specific() {
        let s = SqliteDialect;
        assert_eq!(s.now_iso8601(), "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')");
        assert_eq!(
            s.since_days("listened_at", 7),
            "listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"
        );
        assert_eq!(s.date_trunc_day("listened_at"), "DATE(listened_at)");

        let p = PostgresDialect;
        assert_eq!(
            p.now_iso8601(),
            "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        );
        assert!(
            p.since_days("listened_at", 30)
                .contains("interval '30 days'")
        );
        assert!(
            p.date_trunc_day("listened_at")
                .starts_with("to_char(listened_at::timestamp")
        );
    }
}
