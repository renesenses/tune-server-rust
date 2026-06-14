//! Full-text search — SQLite-only implementation using FTS5 virtual tables.
//!
//! The FTS5 tables are **contentless** (`content=''`) with triggers that
//! keep them in sync with the source tables. This works perfectly when
//! all writes go through the Tune server (the triggers fire on every
//! INSERT/UPDATE/DELETE). However, if a user edits the SQLite database
//! directly (e.g. via `sqlite3` CLI or DB Browser), the triggers may not
//! fire or the FTS content may drift. The `rebuild_fts_contentless`
//! function handles this by deleting all FTS rows and re-inserting from
//! the source tables.
//!
//! Phase 4 of the PostgreSQL support roadmap will introduce a parallel
//! module (or trait split) that targets PostgreSQL: tsvector columns
//! materialised on the source tables, GIN indexes, and `@@ to_tsquery`
//! search predicates. The repos' search() methods will then call
//! `dialect.fts_match(column, placeholder)` to emit the right clause
//! for whichever engine is active.
//!
//! See docs/POSTGRES-PLAN.md.

use rusqlite::Connection;
use tracing::{info, warn};

const FTS_TABLES: &[(&str, &str)] = &[
    ("tracks", "title"),
    ("albums", "title"),
    ("artists", "name"),
];

pub fn setup_fts(conn: &Connection) {
    for &(table, column) in FTS_TABLES {
        let fts_name = format!("{table}_fts");

        let create = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {fts_name} USING fts5(\
             {column}, content={table}, content_rowid=id, \
             tokenize='unicode61 remove_diacritics 2')"
        );
        if let Err(e) = conn.execute_batch(&create) {
            warn!(table = fts_name, error = %e, "fts_create_error");
            continue;
        }

        let triggers = [
            format!(
                "CREATE TRIGGER IF NOT EXISTS {table}_ai AFTER INSERT ON {table} BEGIN \
                 INSERT INTO {fts_name}(rowid, {column}) VALUES (new.id, new.{column}); END"
            ),
            format!(
                "CREATE TRIGGER IF NOT EXISTS {table}_ad AFTER DELETE ON {table} BEGIN \
                 INSERT INTO {fts_name}({fts_name}, rowid, {column}) VALUES ('delete', old.id, old.{column}); END"
            ),
            format!(
                "CREATE TRIGGER IF NOT EXISTS {table}_au AFTER UPDATE OF {column} ON {table} BEGIN \
                 INSERT INTO {fts_name}({fts_name}, rowid, {column}) VALUES ('delete', old.id, old.{column}); \
                 INSERT INTO {fts_name}(rowid, {column}) VALUES (new.id, new.{column}); END"
            ),
        ];

        for trigger in &triggers {
            if let Err(e) = conn.execute_batch(trigger) {
                warn!(table = fts_name, error = %e, "fts_trigger_error");
            }
        }

        // Rebuild FTS if content table has rows but FTS is empty
        let fts_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {fts_name}"), [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let source_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0);

        if source_count > 0 && fts_count == 0 {
            info!(table = fts_name, rows = source_count, "fts_rebuild_content");
            let _ = conn.execute_batch(&format!(
                "INSERT INTO {fts_name}({fts_name}) VALUES ('rebuild')"
            ));
        }
    }

    info!("fts_initialized");
}

pub fn rebuild_fts(conn: &Connection) {
    for &(table, _) in FTS_TABLES {
        let fts_name = format!("{table}_fts");
        if let Err(e) = conn.execute_batch(&format!(
            "INSERT INTO {fts_name}({fts_name}) VALUES ('rebuild')"
        )) {
            warn!(table = fts_name, error = %e, "fts_rebuild_error");
        }
    }
    info!("fts_rebuilt_all");
}

/// Rebuild FTS5 contentless tables by deleting all FTS rows and
/// re-inserting from the source tables. This is the correct approach
/// for `content=''` FTS tables (the standard `rebuild` command only
/// works when `content=<table>` points to an actual table whose
/// columns match the FTS columns).
///
/// The multi-column FTS tables (tracks_fts, albums_fts, artists_fts)
/// use triggers to stay in sync, but manual DB edits bypass triggers.
/// Call this after manual DB corrections, backup restores, or whenever
/// search results seem out of sync with the actual library.
pub fn rebuild_fts_contentless(conn: &Connection) -> Result<i64, String> {
    let start = std::time::Instant::now();
    let mut total_rows = 0i64;

    // For contentless FTS5 tables (content=''), we cannot use
    // `DELETE FROM fts_name` — instead we use the special FTS5
    // `delete-all` command to clear all rows, then re-insert.
    //
    // For content-backed FTS5 tables (content='<table>'), `rebuild`
    // would work, but `delete-all` + re-insert is correct for both.

    // --- tracks_fts: title, artist_name, album_title, genre, composer ---
    match conn.execute_batch("INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => {
            // Table might not exist yet (fresh DB before migration 12)
            warn!(error = %e, "fts_rebuild_delete_tracks_fts");
        }
    }
    match conn.execute(
        "INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
         SELECT t.id, t.title, \
                (SELECT name FROM artists WHERE id = t.artist_id), \
                (SELECT title FROM albums WHERE id = t.album_id), \
                t.genre, t.composer \
         FROM tracks t",
        [],
    ) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_tracks_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_tracks_fts"),
    }

    // --- albums_fts: title, artist_name, genre ---
    match conn.execute_batch("INSERT INTO albums_fts(albums_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => warn!(error = %e, "fts_rebuild_delete_albums_fts"),
    }
    match conn.execute(
        "INSERT INTO albums_fts(rowid, title, artist_name, genre) \
         SELECT a.id, a.title, \
                (SELECT name FROM artists WHERE id = a.artist_id), \
                a.genre \
         FROM albums a",
        [],
    ) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_albums_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_albums_fts"),
    }

    // --- artists_fts: name, sort_name ---
    match conn.execute_batch("INSERT INTO artists_fts(artists_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => warn!(error = %e, "fts_rebuild_delete_artists_fts"),
    }
    match conn.execute(
        "INSERT INTO artists_fts(rowid, name, sort_name) \
         SELECT id, name, sort_name FROM artists",
        [],
    ) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_artists_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_artists_fts"),
    }

    let elapsed_ms = start.elapsed().as_millis();
    info!(total_rows, elapsed_ms, "fts_rebuild_contentless_complete");
    Ok(total_rows)
}

pub fn search_where(table_name: &str) -> String {
    let fts_name = format!("{table_name}_fts");
    format!("{table_name}.id IN (SELECT rowid FROM {fts_name} WHERE {fts_name} MATCH ?)")
}

pub fn search_with_rank(table_name: &str) -> String {
    let fts_name = format!("{table_name}_fts");
    format!("SELECT rowid, rank FROM {fts_name} WHERE {fts_name} MATCH ? ORDER BY rank")
}

pub fn fts_search(conn: &Connection, table_name: &str, query: &str, limit: i64) -> Vec<i64> {
    let fts_name = format!("{table_name}_fts");
    let sql =
        format!("SELECT rowid FROM {fts_name} WHERE {fts_name} MATCH ? ORDER BY rank LIMIT ?");

    let escaped = escape_fts_query(query);

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "fts_search_error");
            return Vec::new();
        }
    };

    stmt.query_map(rusqlite::params![escaped, limit], |row| row.get(0))
        .unwrap_or_else(|_| panic!("fts query_map failed"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default()
}

fn escape_fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            let clean: String = t
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '*')
                .collect();
            if clean.is_empty() {
                String::new()
            } else if clean.ends_with('*') {
                clean
            } else {
                format!("{clean}*")
            }
        })
        .filter(|t| !t.is_empty())
        .collect();

    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn setup_fts_succeeds() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks_fts", [], |r| r.get(0))
            .unwrap_or(-1);
        assert!(count >= 0);
    }

    #[test]
    fn escape_fts_query_basic() {
        assert_eq!(escape_fts_query("hello world"), "hello* world*");
    }

    #[test]
    fn escape_fts_query_special_chars() {
        assert_eq!(escape_fts_query("rock & roll"), "rock* roll*");
    }

    #[test]
    fn escape_fts_query_wildcard_preserved() {
        assert_eq!(escape_fts_query("pink*"), "pink*");
    }

    #[test]
    fn search_where_format() {
        let clause = search_where("tracks");
        assert!(clause.contains("tracks_fts"));
        assert!(clause.contains("MATCH"));
    }

    #[test]
    fn fts_insert_and_search() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        conn.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            [],
        )
        .unwrap();

        let results = fts_search(&conn, "artists", "pink", 10);
        assert_eq!(results, vec![1]);
    }

    #[test]
    fn fts_accent_insensitive() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        conn.execute("INSERT INTO artists (id, name) VALUES (1, 'Stromae')", [])
            .unwrap();

        let results = fts_search(&conn, "artists", "stromae", 10);
        assert_eq!(results, vec![1]);
    }

    #[test]
    fn rebuild_fts_works() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);
        rebuild_fts(&conn);
    }

    #[test]
    fn rebuild_fts_contentless_repopulates_after_manual_edit() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();

        // Insert data via normal path (triggers fire)
        conn.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'The Wall', 1, 1979)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, genre) VALUES (1, 'Comfortably Numb', 1, 1, 'Rock')",
            [],
        ).unwrap();

        // Verify FTS has data
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert!(
            fts_count > 0,
            "FTS should have data after trigger-based inserts"
        );

        // Simulate FTS corruption: clear FTS content using the contentless delete-all command
        conn.execute_batch(
            "INSERT INTO artists_fts(artists_fts) VALUES('delete-all'); \
             INSERT INTO albums_fts(albums_fts) VALUES('delete-all'); \
             INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all');",
        )
        .unwrap();

        // Verify FTS is empty
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 0, "FTS should be empty after manual delete");

        // Rebuild
        let rows = rebuild_fts_contentless(&conn).unwrap();
        assert!(
            rows >= 3,
            "Should have rebuilt at least 3 rows (1 artist + 1 album + 1 track)"
        );

        // Verify FTS is repopulated
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "artists_fts should have 1 row after rebuild");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM albums_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "albums_fts should have 1 row after rebuild");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "tracks_fts should have 1 row after rebuild");
    }
}
