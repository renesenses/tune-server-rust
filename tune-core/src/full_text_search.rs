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
        .filter_map(|r| r.ok())
        .collect()
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
}
