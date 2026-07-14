use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Artist;
use super::sqlite::SqliteDb;
use crate::TuneError;

/// Engine-agnostic SQL builders for artist_repo.
///
/// `COLLATE NOCASE` (SQLite-only) is replaced by `LOWER(col)` for
/// portability. The search() FTS predicate dispatches through
/// `dialect.fts_where` so the same call emits the SQLite FTS5
/// subquery or the Postgres `@@` tsvector predicate.
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

    pub fn list_without_image() -> &'static str {
        "SELECT id, name, musicbrainz_id FROM artists WHERE (image_path IS NULL OR image_path = '') AND musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn list_with_image_and_mbid() -> &'static str {
        "SELECT id, name, musicbrainz_id, image_path FROM artists WHERE image_path IS NOT NULL AND image_path != '' AND musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn list_with_mbid() -> &'static str {
        "SELECT id, name, musicbrainz_id FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn list_all_id_name_mbid() -> &'static str {
        "SELECT id, name, COALESCE(musicbrainz_id, '') FROM artists ORDER BY id"
    }

    pub fn list_without_mbid() -> &'static str {
        "SELECT id, name FROM artists WHERE (musicbrainz_id IS NULL OR musicbrainz_id = '') ORDER BY id"
    }

    pub fn list_without_image_no_mbid() -> &'static str {
        "SELECT id, name FROM artists WHERE (image_path IS NULL OR image_path = '') AND (musicbrainz_id IS NULL OR musicbrainz_id = '') ORDER BY id"
    }

    pub fn list_without_bio() -> &'static str {
        "SELECT id, name, musicbrainz_id FROM artists WHERE (bio IS NULL OR bio = '') AND musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn count_with_bio() -> &'static str {
        "SELECT COUNT(*) FROM artists WHERE bio IS NOT NULL AND bio != ''"
    }

    pub fn list_with_bio_and_mbid() -> &'static str {
        "SELECT id, name, musicbrainz_id, bio, bio_source, bio_source_url, bio_license, bio_lang FROM artists WHERE bio IS NOT NULL AND bio != '' AND musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn list_without_bio_with_mbid() -> &'static str {
        "SELECT id, musicbrainz_id FROM artists WHERE (bio IS NULL OR bio = '') AND musicbrainz_id IS NOT NULL AND musicbrainz_id != '' ORDER BY id"
    }

    pub fn update_mbid<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE artists SET musicbrainz_id = {} WHERE id = {}",
            d.placeholder(2),
            d.placeholder(1)
        )
    }

    /// Engine-agnostic full-text search.
    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM artists a WHERE {} OR LOWER(unaccent(a.name)) LIKE LOWER(unaccent({})) LIMIT {}",
            d.fts_where("artists", "a", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3)
        )
    }
}

pub struct ArtistRepo {
    db: Arc<dyn DbBackend>,
}

impl ArtistRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    pub fn get(&self, id: i64) -> Result<Option<Artist>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_artist))
    }

    pub fn get_by_name(&self, name: &str) -> Result<Option<Artist>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_name, sql::get_by_name);
        let params: [&dyn ToSqlValue; 1] = [&name];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_artist))
    }

    pub fn get_by_musicbrainz_id(&self, mbid: &str) -> Result<Option<Artist>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_musicbrainz_id, sql::get_by_musicbrainz_id);
        let params: [&dyn ToSqlValue; 1] = [&mbid];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_artist))
    }

    pub fn create(&self, artist: &Artist) -> Result<i64, TuneError> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 7] = [
            &artist.name,
            &artist.sort_name,
            &artist.musicbrainz_id,
            &artist.discogs_id,
            &artist.bio,
            &artist.image_path,
            &artist.image_source,
        ];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    /// Sequential `query_one_strong` + `execute` + `last_insert_rowid`
    /// (not `write_tx`) because the scanner holds `BEGIN IMMEDIATE`
    /// while calling this, and `write_tx` would try to start a nested
    /// `BEGIN DEFERRED` — same constraint as `album_repo::get_or_create`.
    ///
    /// Uses `query_one_strong` (write connection) so the SELECT sees
    /// artists created earlier in the same `BEGIN IMMEDIATE` transaction.
    /// Without this, the read-only connection's WAL snapshot misses
    /// uncommitted writes and duplicate artists are created.
    pub fn get_or_create(
        &self,
        name: &str,
        musicbrainz_id: Option<&str>,
        sort_name: Option<&str>,
    ) -> Result<Artist, TuneError> {
        if let Some(mbid) = musicbrainz_id {
            let sql = self.dialect_sql(sql::get_by_musicbrainz_id, sql::get_by_musicbrainz_id);
            let params: [&dyn ToSqlValue; 1] = [&mbid];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                return Ok(row_to_artist(&row));
            }
        }
        {
            let sql = self.dialect_sql(sql::get_by_name, sql::get_by_name);
            let params: [&dyn ToSqlValue; 1] = [&name];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                return Ok(row_to_artist(&row));
            }
        }
        let create_sql = self.dialect_sql(sql::create_minimal, sql::create_minimal);
        let params: [&dyn ToSqlValue; 3] = [&name, &sort_name, &musicbrainz_id];
        self.db.execute(&create_sql, &params)?;
        let id = self.db.last_insert_rowid();
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
    }

    pub fn update(&self, artist: &Artist) -> Result<(), TuneError> {
        let id = artist.id.ok_or("artist has no id")?;
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 8] = [
            &artist.name,
            &artist.sort_name,
            &artist.musicbrainz_id,
            &artist.discogs_id,
            &artist.bio,
            &artist.image_path,
            &artist.image_source,
            &id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, TuneError> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn count_with_bio(&self) -> Result<i64, TuneError> {
        match self.db.query_one(sql::count_with_bio(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Return all artists that have both a bio and a MusicBrainz ID.
    /// Each entry is (name, musicbrainz_id, bio).
    #[allow(clippy::type_complexity)]
    pub fn artists_with_bio_and_mbid(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
        TuneError,
    > {
        let rows = self.db.query_many(sql::list_with_bio_and_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(4).and_then(|v| v.as_string()),
                    cols.get(5).and_then(|v| v.as_string()),
                    cols.get(6).and_then(|v| v.as_string()),
                    cols.get(7).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    /// Return all artists that have a MusicBrainz ID but no local bio.
    /// Used by the community bio download to find candidates for enrichment.
    /// Each entry is (artist_id, musicbrainz_id).
    pub fn artists_without_bio_with_mbid(&self) -> Result<Vec<(i64, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_bio_with_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Artist>, TuneError> {
        let sql = self.dialect_sql(sql::list, sql::list);
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_artist).collect())
    }

    /// Delete artists that have zero tracks referencing them.
    /// Single tx for count-then-delete atomicity.
    pub fn cleanup_orphans(&self) -> Result<i64, TuneError> {
        let mut count: i64 = 0;
        let count_ref = &mut count;
        self.db.write_tx(&mut |tx| {
            *count_ref = tx
                .query_one(sql::count_orphans(), &[])?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if *count_ref > 0 {
                tx.execute(sql::delete_orphans(), &[])?;
            }
            Ok(())
        })?;
        Ok(count)
    }

    /// Return all artists that have a MusicBrainz ID but no image set.
    /// Each entry is (artist_id, name, musicbrainz_id).
    pub fn list_without_image(&self) -> Result<Vec<(i64, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_image(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// All artists that have a MusicBrainz ID, regardless of whether an image
    /// is already set. Used by the "force re-fetch" path to re-pull artwork for
    /// everyone (e.g. when the DB has stale/broken image_path entries that never
    /// render). Each entry is (artist_id, name, musicbrainz_id).
    pub fn list_with_mbid(&self) -> Result<Vec<(i64, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_with_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// All artists as (id, name, mbid) — mbid is `""` when unknown. Used by the
    /// force re-fetch so artists without an MBID are also re-tried (mozaiklabs
    /// by-name + other by-name sources).
    pub fn list_all_id_name_mbid(&self) -> Result<Vec<(i64, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_all_id_name_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Artists that have an image_path AND an MBID. Used to detect the case
    /// where the DB column is set but the cache file is missing (a scan set the
    /// column while the cache write failed, or the cache was later cleared), so
    /// those artists can be re-enriched instead of showing a grey square forever.
    /// Each entry is (artist_id, name, musicbrainz_id, image_path).
    pub fn list_with_image_and_mbid(
        &self,
    ) -> Result<Vec<(i64, String, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_with_image_and_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    pub fn list_without_mbid(&self) -> Result<Vec<(i64, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Return all artists without image AND without MBID.
    /// Each entry is (artist_id, name).
    pub fn list_without_image_no_mbid(&self) -> Result<Vec<(i64, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_image_no_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    pub fn list_without_bio(&self) -> Result<Vec<(i64, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_bio(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    pub fn update_bio(&self, id: i64, bio: &str) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => "UPDATE artists SET bio = ? WHERE id = ?",
            Engine::Postgres => "UPDATE artists SET bio = $1 WHERE id = $2",
        };
        let params: [&dyn ToSqlValue; 2] = [&bio, &id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    /// Update bio together with its provenance (source, URL, license, lang) and
    /// stamp the fetch time. Needed for CC BY-SA attribution and freshness.
    pub fn update_bio_full(
        &self,
        id: i64,
        bio: &str,
        source: &str,
        source_url: Option<String>,
        license: &str,
        lang: &str,
    ) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => {
                "UPDATE artists SET bio = ?, bio_source = ?, bio_source_url = ?, \
                 bio_license = ?, bio_lang = ?, bio_fetched_at = CURRENT_TIMESTAMP WHERE id = ?"
            }
            Engine::Postgres => {
                "UPDATE artists SET bio = $1, bio_source = $2, bio_source_url = $3, \
                 bio_license = $4, bio_lang = $5, bio_fetched_at = CURRENT_TIMESTAMP WHERE id = $6"
            }
        };
        let params: [&dyn ToSqlValue; 6] = [&bio, &source, &source_url, &license, &lang, &id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    /// Bio provenance (source, url, license, lang, fetched_at) for the
    /// artist-detail endpoint. Returns None when no sourced bio is recorded.
    pub fn bio_provenance(&self, id: i64) -> Result<Option<serde_json::Value>, TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => {
                "SELECT bio_source, bio_source_url, bio_license, bio_lang, bio_fetched_at \
                 FROM artists WHERE id = ?"
            }
            Engine::Postgres => {
                "SELECT bio_source, bio_source_url, bio_license, bio_lang, bio_fetched_at \
                 FROM artists WHERE id = $1"
            }
        };
        let params: [&dyn ToSqlValue; 1] = [&id];
        let row = self.db.query_one(sql, &params)?;
        Ok(row.and_then(|cols| {
            let source = cols
                .first()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty())?;
            Some(serde_json::json!({
                "source": source,
                "source_url": cols.get(1).and_then(|v| v.as_string()),
                "license": cols.get(2).and_then(|v| v.as_string()),
                "lang": cols.get(3).and_then(|v| v.as_string()),
                "fetched_at": cols.get(4).and_then(|v| v.as_string()),
            }))
        }))
    }

    pub fn update_mbid(&self, id: i64, mbid: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_mbid, sql::update_mbid);
        self.db.execute(&sql, &[&id as &dyn ToSqlValue, &mbid])?;
        Ok(())
    }

    /// Update only the image_path and image_source for an artist.
    pub fn update_image(&self, id: i64, hash: &str, source: &str) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => "UPDATE artists SET image_path = ?, image_source = ? WHERE id = ?",
            Engine::Postgres => {
                "UPDATE artists SET image_path = $1, image_source = $2 WHERE id = $3"
            }
        };
        let params: [&dyn ToSqlValue; 3] = [&hash, &source, &id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Artist>, TuneError> {
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 3] = [&fts_query, &like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_artist).collect())
    }
}

fn row_to_artist(cols: &Vec<SqlValue>) -> Artist {
    Artist {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        sort_name: cols.get(2).and_then(|v| v.as_string()),
        musicbrainz_id: cols.get(3).and_then(|v| v.as_string()),
        discogs_id: cols.get(4).and_then(|v| v.as_string()),
        bio: cols.get(5).and_then(|v| v.as_string()),
        image_path: cols.get(6).and_then(|v| v.as_string()),
        image_source: cols.get(7).and_then(|v| v.as_string()),
    }
}

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
    fn search_artist_accent_insensitive() {
        let db = test_db();
        let repo = ArtistRepo::new(db);
        repo.create(&Artist::new("Carlão".into())).unwrap();
        repo.create(&Artist::new("Beyoncé".into())).unwrap();

        // Query without accents finds the accented name…
        assert_eq!(repo.search("carlao", 10).unwrap().len(), 1);
        assert_eq!(repo.search("beyonce", 10).unwrap().len(), 1);
        // …and querying WITH accents still works.
        assert_eq!(repo.search("carlão", 10).unwrap().len(), 1);
        // Uppercase accented query folds too (LOWER after unaccent).
        assert_eq!(repo.search("CARLAO", 10).unwrap().len(), 1);
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
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::get_by_name(&s).contains("LOWER(name) = LOWER(?)"));
        assert!(sql::get_by_name(&p).contains("LOWER(name) = LOWER($1)"));
        assert!(!sql::list(&p).contains("COLLATE"));
        assert!(sql::list(&p).contains("LOWER(COALESCE(sort_name, name))"));
    }

    #[test]
    fn search_uses_engine_specific_fts_clause() {
        let s_sql = sql::search(&SqliteDialect);
        assert!(
            s_sql.contains("a.id IN (SELECT rowid FROM artists_fts WHERE artists_fts MATCH ?)")
        );
        let p_sql = sql::search(&PostgresDialect);
        assert!(p_sql.contains("a.search_tsv @@ to_tsquery('simple', unaccent($1))"));
        assert!(s_sql.contains("LOWER(unaccent(a.name)) LIKE LOWER(unaccent(?))"));
        assert!(p_sql.contains("LOWER(unaccent(a.name)) LIKE LOWER(unaccent($2))"));
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

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ArtistRepo::with_backend(backend);
        let id = repo.create(&Artist::new("X".into())).unwrap();
        assert!(repo.get(id).unwrap().is_some());
    }
}
