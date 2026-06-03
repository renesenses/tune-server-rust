use rusqlite::params;

use super::engine::SqlDialect;
use super::models::Artist;
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for artist_repo.
///
/// `COLLATE NOCASE` (SQLite-only) is replaced by `LOWER(col)` for
/// portability. The search() FTS query is SQLite-only for now; phase
/// 4 will inject a `dialect.fts_match` clause and emit a PG-shape
/// query against a tsvector column.
pub mod sql {
    use super::SqlDialect;

    const COLS: &str =
        "id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("SELECT {COLS} FROM artists WHERE id = {}", d.placeholder(1))
    }

    pub fn get_by_name<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM artists WHERE LOWER(name) = LOWER({})",
            d.placeholder(1)
        )
    }

    pub fn get_by_musicbrainz_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM artists WHERE musicbrainz_id = {}",
            d.placeholder(1)
        )
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO artists (name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source) VALUES ({}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7)
        )
    }

    pub fn create_minimal<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO artists (name, sort_name, musicbrainz_id) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE artists SET name = {}, sort_name = {}, musicbrainz_id = {}, discogs_id = {}, bio = {}, image_path = {}, image_source = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM artists WHERE id = {}", d.placeholder(1))
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)"
    }

    pub fn list<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL) ORDER BY LOWER(COALESCE(sort_name, name)) LIMIT {} OFFSET {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_orphans() -> &'static str {
        "SELECT COUNT(*) FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM tracks WHERE artist_id IS NOT NULL)"
    }

    pub fn delete_orphans() -> &'static str {
        "DELETE FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM tracks WHERE artist_id IS NOT NULL)"
    }

    /// Engine-agnostic full-text search. The FTS predicate is built
    /// via `dialect.fts_where` so the same Rust call site emits the
    /// SQLite FTS5 subquery on SQLite and the tsvector `@@` predicate
    /// on Postgres (see tune-core/migrations/postgres/002_fts_tsvector.sql).
    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM artists a WHERE {} OR LOWER(a.name) LIKE LOWER({}) LIMIT {}",
            d.fts_where("artists", "a", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3)
        )
    }
}

pub struct ArtistRepo {
    db: SqliteDb,
}

impl ArtistRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Artist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::get_by_id(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![id], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn get_by_name(&self, name: &str) -> Result<Option<Artist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::get_by_name(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![name], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn get_by_musicbrainz_id(&self, mbid: &str) -> Result<Option<Artist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::get_by_musicbrainz_id(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![mbid], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn create(&self, artist: &Artist) -> Result<i64, String> {
        self.db.execute(
            &sql::create(&self.db.dialect()),
            &[
                &artist.name as &dyn rusqlite::types::ToSql,
                &artist.sort_name,
                &artist.musicbrainz_id,
                &artist.discogs_id,
                &artist.bio,
                &artist.image_path,
                &artist.image_source,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Look up or create an artist, using the write connection for the entire
    /// read-then-write sequence. This ensures that during a scan transaction
    /// (BEGIN IMMEDIATE), the lookup sees artists created earlier in the same
    /// batch, preventing duplicates.
    pub fn get_or_create(
        &self,
        name: &str,
        musicbrainz_id: Option<&str>,
        sort_name: Option<&str>,
    ) -> Result<Artist, String> {
        let d = self.db.dialect();
        let by_mbid_sql = sql::get_by_musicbrainz_id(&d);
        let by_name_sql = sql::get_by_name(&d);
        let create_sql = sql::create_minimal(&d);
        self.db.write(|conn| {
            if let Some(mbid) = musicbrainz_id {
                let mut stmt = conn.prepare_cached(&by_mbid_sql)?;
                if let Some(artist) = stmt
                    .query_row(params![mbid], |row| Ok(row_to_artist(row)))
                    .optional()?
                {
                    return Ok(artist);
                }
            }
            {
                let mut stmt = conn.prepare_cached(&by_name_sql)?;
                if let Some(artist) = stmt
                    .query_row(params![name], |row| Ok(row_to_artist(row)))
                    .optional()?
                {
                    return Ok(artist);
                }
            }
            conn.execute(
                &create_sql,
                rusqlite::params![name, sort_name, musicbrainz_id],
            )?;
            let id = conn.last_insert_rowid();
            Ok(Artist {
                id: Some(id),
                name: name.to_string(),
                sort_name: sort_name.map(|s| s.to_string()),
                musicbrainz_id: musicbrainz_id.map(|s| s.to_string()),
                discogs_id: None,
                bio: None,
                image_path: None,
                image_source: None,
            })
        })
    }

    pub fn update(&self, artist: &Artist) -> Result<(), String> {
        let id = artist.id.ok_or("artist has no id")?;
        self.db.execute(
            &sql::update(&self.db.dialect()),
            &[
                &artist.name as &dyn rusqlite::types::ToSql,
                &artist.sort_name,
                &artist.musicbrainz_id,
                &artist.discogs_id,
                &artist.bio,
                &artist.image_path,
                &artist.image_source,
                &id,
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute(&sql::delete(&self.db.dialect()), &[&id])?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row(sql::count(), [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Artist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::list(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        let artists = stmt
            .query_map(params![limit, offset], |row| Ok(row_to_artist(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(artists)
    }

    /// Delete artists that have zero tracks referencing them.
    pub fn cleanup_orphans(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        let count: i64 = conn
            .query_row(sql::count_orphans(), [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if count > 0 {
            conn.execute(sql::delete_orphans(), [])
                .map_err(|e| e.to_string())?;
        }
        Ok(count)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Artist>, String> {
        // Engine-specific tokenization + prefix wrapping.
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::search(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        let artists = stmt
            .query_map(params![fts_query, like, limit], |row| {
                Ok(row_to_artist(row))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(artists)
    }
}

fn row_to_artist(row: &rusqlite::Row) -> Artist {
    Artist {
        id: row.get(0).ok(),
        name: row.get(1).unwrap_or_default(),
        sort_name: row.get(2).ok(),
        musicbrainz_id: row.get(3).ok(),
        discogs_id: row.get(4).ok(),
        bio: row.get(5).ok(),
        image_path: row.get(6).ok(),
        image_source: row.get(7).ok(),
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    fn link_artist_album(db: &SqliteDb, artist_id: i64) {
        let conn = db.connection().lock().unwrap();
        conn.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('test album', ?)",
            rusqlite::params![artist_id],
        )
        .ok();
    }

    #[test]
    fn crud_artist() {
        let db = test_db();
        let repo = ArtistRepo::new(db);

        let mut artist = Artist::new("Pink Floyd".into());
        let id = repo.create(&artist).unwrap();
        assert!(id > 0);

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.name, "Pink Floyd");

        artist.id = Some(id);
        artist.bio = Some("English rock band".into());
        repo.update(&artist).unwrap();

        let updated = repo.get(id).unwrap().unwrap();
        assert_eq!(updated.bio.as_deref(), Some("English rock band"));

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn get_or_create() {
        let db = test_db();
        let repo = ArtistRepo::new(db.clone());

        let a1 = repo.get_or_create("Beatles", None, None).unwrap();
        link_artist_album(&db, a1.id.unwrap());
        let a2 = repo.get_or_create("Beatles", None, None).unwrap();
        assert_eq!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn search_artist() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        repo.create(&Artist::new("Pink Floyd".into())).unwrap();
        repo.create(&Artist::new("Led Zeppelin".into())).unwrap();

        let results = repo.search("floyd", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Pink Floyd");
    }

    #[test]
    fn artist_count() {
        let db = test_db();
        let repo = ArtistRepo::new(db.clone());

        assert_eq!(repo.count().unwrap(), 0);
        let a1 = repo.create(&Artist::new("A".into())).unwrap();
        let a2 = repo.create(&Artist::new("B".into())).unwrap();
        link_artist_album(&db, a1);
        link_artist_album(&db, a2);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn artist_list() {
        let db = test_db();
        let repo = ArtistRepo::new(db.clone());

        let a1 = repo.create(&Artist::new("Zappa".into())).unwrap();
        let a2 = repo.create(&Artist::new("Armstrong".into())).unwrap();
        let a3 = repo.create(&Artist::new("Miles Davis".into())).unwrap();
        link_artist_album(&db, a1);
        link_artist_album(&db, a2);
        link_artist_album(&db, a3);

        let all = repo.list(100, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "Armstrong");
    }

    #[test]
    fn artist_list_pagination() {
        let db = test_db();
        let repo = ArtistRepo::new(db.clone());

        for i in 0..10 {
            let a = repo.create(&Artist::new(format!("Artist {i:02}"))).unwrap();
            link_artist_album(&db, a);
        }

        let page1 = repo.list(3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = repo.list(3, 3).unwrap();
        assert_eq!(page2.len(), 3);
        assert_ne!(page1[0].name, page2[0].name);
    }

    #[test]
    fn artist_get_by_name_case_insensitive() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        repo.create(&Artist::new("Miles Davis".into())).unwrap();

        let found = repo.get_by_name("miles davis").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Miles Davis");
    }

    #[test]
    fn artist_get_by_musicbrainz_id() {
        let db = test_db();
        let repo = ArtistRepo::new(db);

        let mut artist = Artist::new("Miles Davis".into());
        artist.musicbrainz_id = Some("561d854a-6a28-4aa7-8c99-323e6ce46c2a".into());
        repo.create(&artist).unwrap();

        let found = repo
            .get_by_musicbrainz_id("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Miles Davis");
    }

    #[test]
    fn artist_get_or_create_with_musicbrainz_id() {
        let db = test_db();
        let repo = ArtistRepo::new(db.clone());

        let a1 = repo
            .get_or_create("Miles Davis", Some("mbid-123"), None)
            .unwrap();
        link_artist_album(&db, a1.id.unwrap());
        let a2 = repo
            .get_or_create("Miles Davis", Some("mbid-123"), None)
            .unwrap();
        assert_eq!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn artist_with_sort_name() {
        let db = test_db();
        let repo = ArtistRepo::new(db);

        let a = repo
            .get_or_create("The Beatles", None, Some("Beatles, The"))
            .unwrap();
        assert_eq!(a.sort_name.as_deref(), Some("Beatles, The"));
    }

    #[test]
    fn artist_update_all_fields() {
        let db = test_db();
        let repo = ArtistRepo::new(db);

        let id = repo.create(&Artist::new("Test".into())).unwrap();
        let mut artist = repo.get(id).unwrap().unwrap();
        artist.sort_name = Some("Test, The".into());
        artist.musicbrainz_id = Some("mbid-999".into());
        artist.discogs_id = Some("disco-123".into());
        artist.bio = Some("A test artist".into());
        artist.image_path = Some("/img/test.jpg".into());
        artist.image_source = Some("lastfm".into());
        repo.update(&artist).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.sort_name.as_deref(), Some("Test, The"));
        assert_eq!(fetched.musicbrainz_id.as_deref(), Some("mbid-999"));
        assert_eq!(fetched.bio.as_deref(), Some("A test artist"));
    }

    #[test]
    fn artist_unicode_name() {
        let db = test_db();
        let repo = ArtistRepo::new(db);

        let id = repo.create(&Artist::new("Bjork".into())).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.name, "Bjork");
    }

    #[test]
    fn artist_get_nonexistent() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn artist_get_by_name_nonexistent() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        assert!(repo.get_by_name("Nobody").unwrap().is_none());
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        use crate::db::engine::{PostgresDialect, SqliteDialect};
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::get_by_name(&s).contains("LOWER(name) = LOWER(?)"));
        assert!(sql::get_by_name(&p).contains("LOWER(name) = LOWER($1)"));
        assert!(!sql::list(&p).contains("COLLATE"));
        assert!(sql::list(&p).contains("LOWER(COALESCE(sort_name, name))"));
    }

    #[test]
    fn search_uses_engine_specific_fts_clause() {
        use crate::db::engine::{PostgresDialect, SqliteDialect};
        // SQLite branch keeps the FTS5 subquery against artists_fts.
        let s_sql = sql::search(&SqliteDialect);
        assert!(
            s_sql.contains("a.id IN (SELECT rowid FROM artists_fts WHERE artists_fts MATCH ?)")
        );
        // PG branch hits the tsvector column directly.
        let p_sql = sql::search(&PostgresDialect);
        assert!(p_sql.contains("a.search_tsv @@ to_tsquery('simple', unaccent($1))"));
        // Both share the LIKE fallback for fuzzy name matches.
        assert!(s_sql.contains("LOWER(a.name) LIKE LOWER(?)"));
        assert!(p_sql.contains("LOWER(a.name) LIKE LOWER($2)"));
    }

    #[test]
    fn artist_search_multiple() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        repo.create(&Artist::new("Jazz Artist".into())).unwrap();
        repo.create(&Artist::new("Jazz Trio".into())).unwrap();
        repo.create(&Artist::new("Rock Band".into())).unwrap();

        let results = repo.search("Jazz", 10).unwrap();
        assert_eq!(results.len(), 2);
    }
}
