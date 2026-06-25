use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Album;
use super::sqlite::SqliteDb;
use crate::TuneError;

/// Engine-agnostic SQL builders for album_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn select_album() -> &'static str {
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.original_year, a.genre, a.disc_count, a.track_count, a.cover_path, a.source, a.source_id, a.label, a.catalog_number, a.barcode, a.format, a.sample_rate, a.bit_depth, a.bio, a.musicbrainz_release_id, a.musicbrainz_release_group_id, a.release_date, a.original_date, a.genres FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id"
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("{} WHERE a.id = {}", select_album(), d.placeholder(1))
    }

    pub fn get_by_title<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) LIMIT 1",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn get_by_title_only<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.title = {} LIMIT 1",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn get_by_title_artist_year<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) AND a.artist_id = {} AND a.year = {}",
            select_album(),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_by_title_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) AND a.artist_id = {}",
            select_album(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_by_musicbrainz_release_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.musicbrainz_release_id = {}",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year, original_year, genre, genres, disc_count, track_count, cover_path, source, source_id, label, catalog_number, barcode, format, sample_rate, bit_depth, bio, musicbrainz_release_id, musicbrainz_release_group_id, release_date, original_date) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
            d.placeholder(13),
            d.placeholder(14),
            d.placeholder(15),
            d.placeholder(16),
            d.placeholder(17),
            d.placeholder(18),
            d.placeholder(19),
            d.placeholder(20),
            d.placeholder(21),
            d.placeholder(22),
        )
    }

    pub fn create_minimal<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn create_with_mbid<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year, musicbrainz_release_id) VALUES ({}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET title = {}, artist_id = {}, year = {}, original_year = {}, genre = {}, genres = {}, disc_count = {}, track_count = {}, cover_path = {}, label = {}, catalog_number = {}, format = {}, sample_rate = {}, bit_depth = {}, bio = {}, musicbrainz_release_id = {}, musicbrainz_release_group_id = {}, release_date = {}, original_date = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
            d.placeholder(13),
            d.placeholder(14),
            d.placeholder(15),
            d.placeholder(16),
            d.placeholder(17),
            d.placeholder(18),
            d.placeholder(19),
            d.placeholder(20),
        )
    }

    /// Update album date fields using COALESCE so we only fill in
    /// values that are not already set.
    pub fn update_dates<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET original_year = COALESCE(original_year, {}), release_date = COALESCE(release_date, {}), original_date = COALESCE(original_date, {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn update_cover_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET cover_path = COALESCE(cover_path, {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn force_update_cover_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET cover_path = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_track_count<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET track_count = (SELECT COUNT(*) FROM tracks WHERE album_id = {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM albums WHERE id = {}", d.placeholder(1))
    }

    pub fn count_orphans() -> &'static str {
        "SELECT COUNT(*) FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)"
    }

    pub fn delete_orphans() -> &'static str {
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)"
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM albums"
    }

    pub fn list_recent<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} ORDER BY a.id DESC LIMIT {}",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn list_by_release_group<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.musicbrainz_release_group_id = {} ORDER BY a.year ASC, LOWER(a.title) ASC",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn list_release_groups() -> String {
        format!(
            "{} WHERE a.musicbrainz_release_group_id IS NOT NULL ORDER BY a.musicbrainz_release_group_id, a.year ASC",
            select_album()
        )
    }

    pub fn list_by_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.artist_id = {} ORDER BY a.year ASC, LOWER(a.title) ASC",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn list_without_cover() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_id FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.cover_path IS NULL OR a.cover_path = '') AND a.source = 'local' ORDER BY a.id"
    }

    pub fn list_without_bio() -> &'static str {
        "SELECT a.id, a.title, ar.name FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.bio IS NULL OR a.bio = '') AND a.source = 'local' ORDER BY a.id"
    }

    pub fn count_with_bio() -> &'static str {
        "SELECT COUNT(*) FROM albums WHERE bio IS NOT NULL AND bio != ''"
    }

    pub fn list_with_bio_and_mbid() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_group_id, a.bio FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE a.bio IS NOT NULL AND a.bio != '' AND a.musicbrainz_release_group_id IS NOT NULL AND a.musicbrainz_release_group_id != '' ORDER BY a.id"
    }

    pub fn list_with_bio() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_group_id, a.bio FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE a.bio IS NOT NULL AND a.bio != '' ORDER BY a.id"
    }

    pub fn list_without_bio_with_mbid() -> &'static str {
        "SELECT a.id, a.musicbrainz_release_group_id FROM albums a WHERE (a.bio IS NULL OR a.bio = '') AND a.musicbrainz_release_group_id IS NOT NULL AND a.musicbrainz_release_group_id != '' ORDER BY a.id"
    }

    pub fn list_without_bio_without_mbid() -> &'static str {
        "SELECT a.id, a.title, ar.name FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.bio IS NULL OR a.bio = '') AND (a.musicbrainz_release_group_id IS NULL OR a.musicbrainz_release_group_id = '') AND a.source = 'local' ORDER BY a.id"
    }

    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE ({}) OR LOWER(a.title) LIKE LOWER({}) OR LOWER(ar.name) LIKE LOWER({}) OR LOWER(a.genre) LIKE LOWER({}) OR a.musicbrainz_release_id = {} OR EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND LOWER(t.title) LIKE LOWER({})) LIMIT {}",
            select_album(),
            d.fts_where("albums", "a", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7)
        )
    }
}

pub struct AlbumRepo {
    db: Arc<dyn DbBackend>,
}

impl AlbumRepo {
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

    pub fn get(&self, id: i64) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn get_by_title(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title, sql::get_by_title);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    /// Like `get_by_title` but reads through the write connection.
    /// Used by the scanner when running inside a `BEGIN IMMEDIATE` to
    /// see albums created earlier in the same transaction.
    pub fn get_by_title_strong(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title, sql::get_by_title);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn get_by_title_and_artist(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Option<Album>, TuneError> {
        if let Some(y) = year {
            let sql =
                self.dialect_sql(sql::get_by_title_artist_year, sql::get_by_title_artist_year);
            let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &y];
            if let Some(row) = self.db.query_one(&sql, &params)? {
                return Ok(Some(row_to_album(&row)));
            }
        }
        let sql = self.dialect_sql(sql::get_by_title_artist, sql::get_by_title_artist);
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn get_by_title_only(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title_only, sql::get_by_title_only);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    /// Like `get_by_title_only` but reads through the write connection.
    /// Used by the scanner when running inside a `BEGIN IMMEDIATE` to
    /// see albums created earlier in the same transaction.
    pub fn get_by_title_only_strong(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title_only, sql::get_by_title_only);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn get_by_musicbrainz_release_id(
        &self,
        release_id: &str,
    ) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(
            sql::get_by_musicbrainz_release_id,
            sql::get_by_musicbrainz_release_id,
        );
        let params: [&dyn ToSqlValue; 1] = [&release_id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn create(&self, album: &Album) -> Result<i64, TuneError> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 22] = [
            &album.title,
            &album.artist_id,
            &album.year,
            &album.original_year,
            &album.genre,
            &album.genres,
            &album.disc_count,
            &album.track_count,
            &album.cover_path,
            &album.source,
            &album.source_id,
            &album.label,
            &album.catalog_number,
            &album.barcode,
            &album.format,
            &album.sample_rate,
            &album.bit_depth,
            &album.bio,
            &album.musicbrainz_release_id,
            &album.musicbrainz_release_group_id,
            &album.release_date,
            &album.original_date,
        ];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    /// Look up an album by (title, artist, year), or create it.
    /// Sequential `query_one_strong` + `execute` + `last_insert_rowid`
    /// (not `write_tx`) because the scanner holds `BEGIN IMMEDIATE`
    /// while calling this, and `write_tx` would try to start a nested
    /// `BEGIN DEFERRED` — same constraint as `zone_repo::create` (cf.
    /// commit `9f502c0`). On SQLite the write mutex serializes the
    /// three calls, so a concurrent `get_or_create` on another thread
    /// can't shift the rowid we read.
    ///
    /// Uses `query_one_strong` (write connection) instead of
    /// `query_one` (read pool) so that the SELECT sees albums created
    /// earlier in the same `BEGIN IMMEDIATE` transaction. Without this,
    /// the read-only connection's WAL snapshot does not include
    /// uncommitted writes, causing each track in a batch to create a
    /// separate album instead of reusing the one created by the first
    /// track.
    pub fn get_or_create(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Album, TuneError> {
        if let Some(found) = self.find_by_title_and_artist_strong(title, artist_id, year)? {
            return Ok(found);
        }
        let create_sql = self.dialect_sql(sql::create_minimal, sql::create_minimal);
        let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &year];
        self.db.execute(&create_sql, &params)?;
        let id = self.db.last_insert_rowid();
        let mut album = Album::new(title.to_string());
        album.id = Some(id);
        album.artist_id = Some(artist_id);
        album.year = year;
        Ok(album)
    }

    /// Like `get_or_create` but also checks MusicBrainz release ID
    /// first.
    ///
    /// Lookup cascade:
    /// 1. MusicBrainz release ID (exact match)
    /// 2. Title + artist_id (+ year if present) — case-insensitive title
    pub fn get_or_create_with_mbid(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        mbid: Option<&str>,
    ) -> Result<Album, TuneError> {
        if let Some(release_id) = mbid {
            let sql = self.dialect_sql(
                sql::get_by_musicbrainz_release_id,
                sql::get_by_musicbrainz_release_id,
            );
            let params: [&dyn ToSqlValue; 1] = [&release_id];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                return Ok(row_to_album(&row));
            }
        }
        if let Some(found) = self.find_by_title_and_artist_strong(title, artist_id, year)? {
            return Ok(found);
        }
        let create_sql = self.dialect_sql(sql::create_with_mbid, sql::create_with_mbid);
        let params: [&dyn ToSqlValue; 4] = [&title, &artist_id, &year, &mbid];
        self.db.execute(&create_sql, &params)?;
        let id = self.db.last_insert_rowid();
        let mut album = Album::new(title.to_string());
        album.id = Some(id);
        album.artist_id = Some(artist_id);
        album.year = year;
        album.musicbrainz_release_id = mbid.map(String::from);
        Ok(album)
    }

    /// Like `get_by_title_and_artist` but uses `query_one_strong` to
    /// read through the write connection. Called by `get_or_create` /
    /// `get_or_create_with_mbid` which run inside a scanner
    /// `BEGIN IMMEDIATE` transaction.
    fn find_by_title_and_artist_strong(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Option<Album>, TuneError> {
        if let Some(y) = year {
            let sql =
                self.dialect_sql(sql::get_by_title_artist_year, sql::get_by_title_artist_year);
            let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &y];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                return Ok(Some(row_to_album(&row)));
            }
        }
        let sql = self.dialect_sql(sql::get_by_title_artist, sql::get_by_title_artist);
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn update(&self, album: &Album) -> Result<(), TuneError> {
        let id = album.id.ok_or("album has no id")?;
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 20] = [
            &album.title,
            &album.artist_id,
            &album.year,
            &album.original_year,
            &album.genre,
            &album.genres,
            &album.disc_count,
            &album.track_count,
            &album.cover_path,
            &album.label,
            &album.catalog_number,
            &album.format,
            &album.sample_rate,
            &album.bit_depth,
            &album.bio,
            &album.musicbrainz_release_id,
            &album.musicbrainz_release_group_id,
            &album.release_date,
            &album.original_date,
            &id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Set album date fields (original_year, release_date, original_date)
    /// using COALESCE — only fills in values not already set.
    pub fn update_dates(
        &self,
        album_id: i64,
        original_year: Option<i32>,
        release_date: Option<&str>,
        original_date: Option<&str>,
    ) -> Result<(), TuneError> {
        // Skip if all values are None — nothing to update.
        if original_year.is_none() && release_date.is_none() && original_date.is_none() {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::update_dates, sql::update_dates);
        let params: [&dyn ToSqlValue; 4] =
            [&original_year, &release_date, &original_date, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_cover_path(&self, album_id: i64, cover_path: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_cover_path, sql::update_cover_path);
        let params: [&dyn ToSqlValue; 2] = [&cover_path, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Like `update_cover_path` but always overwrites the existing value.
    /// Used by rescan endpoints where the user explicitly wants to refresh artwork.
    pub fn force_update_cover_path(
        &self,
        album_id: i64,
        cover_path: &str,
    ) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::force_update_cover_path, sql::force_update_cover_path);
        let params: [&dyn ToSqlValue; 2] = [&cover_path, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_track_count(&self, album_id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_track_count, sql::update_track_count);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_quality_from_tracks(&self, album_id: i64) -> Result<(), TuneError> {
        // 7 references to the same album_id parameter. SQLite uses `?`
        // for each; PG would use $1..$7 — we build the placeholder list
        // via the dialect to keep both engines happy.
        let p = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let plist = (1..=7)
            .map(|i| match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(i),
                Engine::Postgres => PostgresDialect.placeholder(i),
            })
            .collect::<Vec<_>>();
        let _ = p;
        let sql = format!(
            "UPDATE albums SET
                format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = {} AND t.format IS NOT NULL LIMIT 1)),
                sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = {})),
                bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = {})),
                genre = COALESCE(albums.genre, (SELECT t.genre FROM tracks t WHERE t.album_id = {} AND t.genre IS NOT NULL LIMIT 1)),
                genres = COALESCE(albums.genres, (SELECT t.genres FROM tracks t WHERE t.album_id = {} AND t.genres IS NOT NULL LIMIT 1)),
                disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = {}))
            WHERE id = {}",
            plist[0], plist[1], plist[2], plist[3], plist[4], plist[5], plist[6]
        );
        let params: [&dyn ToSqlValue; 7] = [
            &album_id, &album_id, &album_id, &album_id, &album_id, &album_id, &album_id,
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

    pub fn delete_orphans(&self) -> Result<i64, TuneError> {
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

    /// Return all albums that have both a bio and a MusicBrainz release group ID.
    /// Each entry is (title, artist_name, musicbrainz_release_group_id, bio).
    pub fn albums_with_bio_and_mbid(
        &self,
    ) -> Result<Vec<(String, Option<String>, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_with_bio_and_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Return all albums that have a non-empty bio, regardless of MBID.
    /// Each entry is (title, artist_name, musicbrainz_release_group_id, bio).
    /// The MBID may be None for albums without a MusicBrainz ID.
    pub fn albums_with_bio(
        &self,
    ) -> Result<Vec<(String, Option<String>, Option<String>, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_with_bio(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()),
                    cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Return all albums that have a MusicBrainz release group ID but no local bio.
    /// Used by the community bio download to find candidates for enrichment.
    /// Each entry is (album_id, musicbrainz_release_group_id).
    pub fn albums_without_bio_with_mbid(&self) -> Result<Vec<(i64, String)>, TuneError> {
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

    /// Return all local albums without bio and without MBID.
    /// Used by the community bio download to find candidates for title+artist lookup.
    /// Each entry is (album_id, title, artist_name).
    pub fn albums_without_bio_without_mbid(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>)>, TuneError> {
        let rows = self
            .db
            .query_many(sql::list_without_bio_without_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_recent, sql::list_recent);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    pub fn list_by_release_group(&self, group_id: &str) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_release_group, sql::list_by_release_group);
        let params: [&dyn ToSqlValue; 1] = [&group_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    pub fn list_release_groups(&self) -> Result<Vec<(String, Vec<Album>)>, TuneError> {
        let rows = self.db.query_many(&sql::list_release_groups(), &[])?;
        let albums: Vec<Album> = rows.iter().map(row_to_album).collect();

        let mut groups: std::collections::HashMap<String, Vec<Album>> =
            std::collections::HashMap::new();
        for album in albums {
            if let Some(ref gid) = album.musicbrainz_release_group_id {
                groups.entry(gid.clone()).or_default().push(album);
            }
        }
        Ok(groups.into_iter().filter(|(_, v)| v.len() > 1).collect())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Album>, TuneError> {
        self.list_sorted(limit, offset, "title", "asc")
    }

    pub fn list_sorted(
        &self,
        limit: i64,
        offset: i64,
        sort: &str,
        order: &str,
    ) -> Result<Vec<Album>, TuneError> {
        self.list_filtered(limit, offset, sort, order, None, None)
    }

    pub fn list_filtered(
        &self,
        limit: i64,
        offset: i64,
        sort: &str,
        order: &str,
        format: Option<&str>,
        quality: Option<&str>,
    ) -> Result<Vec<Album>, TuneError> {
        let dir = if order.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            "ASC"
        };
        let order_clause = match sort {
            "title" => format!("LOWER(a.title) {dir}"),
            "release_date" => format!(
                "COALESCE(a.release_date, a.original_date, CAST(a.year AS TEXT)) {dir} NULLS LAST, LOWER(a.title) ASC"
            ),
            "year" => format!("a.year {dir} NULLS LAST, LOWER(a.title) ASC"),
            "artist" => {
                format!("LOWER(ar.name) {dir}, a.year ASC, LOWER(a.title) ASC")
            }
            "added_at" => format!("a.id {dir}"),
            _ => format!("a.id {dir}"),
        };

        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };

        let mut wheres: Vec<String> = Vec::new();
        let mut bind_values: Vec<SqlValue> = Vec::new();
        let mut next_ph = 1usize;

        if let Some(fmt) = format {
            wheres.push(format!(
                "a.id IN (SELECT DISTINCT album_id FROM tracks WHERE format = {})",
                make_ph(next_ph)
            ));
            bind_values.push(SqlValue::Text(fmt.to_string()));
            next_ph += 1;
        }
        match quality {
            Some("dsd") => {
                wheres.push("a.id IN (SELECT DISTINCT album_id FROM tracks WHERE format IN ('dsd','dsf','dff'))".to_string());
            }
            Some("hires") => {
                wheres.push("a.id IN (SELECT DISTINCT album_id FROM tracks WHERE sample_rate > 44100 OR bit_depth > 16)".to_string());
            }
            Some("cd") => {
                wheres.push("(a.sample_rate = 44100 AND a.bit_depth = 16)".to_string());
            }
            Some("lossy") => {
                wheres.push("a.format IN ('mp3','aac','ogg','opus','wma')".to_string());
            }
            _ => {}
        }

        let where_clause = if wheres.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wheres.join(" AND "))
        };

        let limit_ph = make_ph(next_ph);
        next_ph += 1;
        let offset_ph = make_ph(next_ph);

        let sql = format!(
            "{}{where_clause} ORDER BY {order_clause} LIMIT {limit_ph} OFFSET {offset_ph}",
            sql::select_album()
        );

        bind_values.push(SqlValue::Int(limit));
        bind_values.push(SqlValue::Int(offset));

        let refs: Vec<&dyn ToSqlValue> = bind_values.iter().map(|v| v as &dyn ToSqlValue).collect();
        let rows = self.db.query_many(&sql, &refs)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_artist, sql::list_by_artist);
        let params: [&dyn ToSqlValue; 1] = [&artist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    /// Match albums where `genre` appears in either the legacy
    /// delimiter-separated text column or the structured `genres`
    /// JSON array (via `dialect.json_array_contains_lower`). Now
    /// PG-compatible.
    pub fn list_by_genre(&self, genre: &str) -> Result<Vec<Album>, TuneError> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let json_contains = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.json_array_contains_lower("a.genres", &make_ph(2)),
            Engine::Postgres => PostgresDialect.json_array_contains_lower("a.genres", &make_ph(2)),
        };
        let delimited_pattern = format!("%,{},%", genre.replace('%', "").replace('_', ""));
        let sql = format!(
            "{} WHERE \
             LOWER(',' || REPLACE(REPLACE(REPLACE(REPLACE(a.genre, '; ', ','), ';', ','), '/ ', ','), '/', ',') || ',') LIKE LOWER({}) \
             OR {} \
             ORDER BY LOWER(a.title)",
            sql::select_album(),
            make_ph(1),
            json_contains,
        );
        let params: [&dyn ToSqlValue; 2] = [&delimited_pattern, &genre];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    /// Return all local albums that have no cover art set.
    /// Each entry is (album_id, title, artist_name, musicbrainz_release_id).
    #[allow(clippy::type_complexity)]
    pub fn list_without_cover(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>, Option<String>)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_cover(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    /// Return all local albums without bio.
    /// Each entry is (album_id, title, artist_name).
    pub fn list_without_bio(&self) -> Result<Vec<(i64, String, Option<String>)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_bio(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    pub fn update_bio(&self, album_id: i64, bio: &str) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => "UPDATE albums SET bio = ? WHERE id = ?",
            Engine::Postgres => "UPDATE albums SET bio = $1 WHERE id = $2",
        };
        let params: [&dyn ToSqlValue; 2] = [&bio, &album_id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Album>, TuneError> {
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let trimmed = query.trim();
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 7] =
            [&fts_query, &like, &like, &like, &trimmed, &like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }
}

fn row_to_album(cols: &Vec<SqlValue>) -> Album {
    Album {
        id: cols.first().and_then(|v| v.as_i64()),
        title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        artist_id: cols.get(2).and_then(|v| v.as_i64()),
        artist_name: cols.get(3).and_then(|v| v.as_string()),
        year: cols.get(4).and_then(|v| v.as_i64()).map(|n| n as i32),
        original_year: cols.get(5).and_then(|v| v.as_i64()).map(|n| n as i32),
        genre: cols.get(6).and_then(|v| v.as_string()),
        // Index 23 (after the 23-col select): a.genres
        genres: cols.get(23).and_then(|v| v.as_string()),
        disc_count: cols.get(7).and_then(|v| v.as_i64()).map(|n| n as i32),
        track_count: cols.get(8).and_then(|v| v.as_i64()).map(|n| n as i32),
        cover_path: cols.get(9).and_then(|v| v.as_string()),
        source: cols
            .get(10)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "local".into()),
        source_id: cols.get(11).and_then(|v| v.as_string()),
        label: cols.get(12).and_then(|v| v.as_string()),
        catalog_number: cols.get(13).and_then(|v| v.as_string()),
        barcode: cols.get(14).and_then(|v| v.as_string()),
        format: cols.get(15).and_then(|v| v.as_string()),
        sample_rate: cols.get(16).and_then(|v| v.as_i64()).map(|n| n as i32),
        bit_depth: cols.get(17).and_then(|v| v.as_i64()).map(|n| n as i32),
        bio: cols.get(18).and_then(|v| v.as_string()),
        musicbrainz_release_id: cols.get(19).and_then(|v| v.as_string()),
        musicbrainz_release_group_id: cols.get(20).and_then(|v| v.as_string()),
        release_date: cols.get(21).and_then(|v| v.as_string()),
        original_date: cols.get(22).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn crud_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let artist_id = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let mut album = Album::new("The Dark Side of the Moon".into());
        album.artist_id = Some(artist_id);
        album.year = Some(1973);

        let id = repo.create(&album).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "The Dark Side of the Moon");
        assert_eq!(fetched.artist_name.as_deref(), Some("Pink Floyd"));
        assert_eq!(fetched.year, Some(1973));

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn get_or_create_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let artist_id = artist_repo.create(&Artist::new("Beatles".into())).unwrap();
        let a1 = repo
            .get_or_create("Abbey Road", artist_id, Some(1969))
            .unwrap();
        let a2 = repo
            .get_or_create("Abbey Road", artist_id, Some(1969))
            .unwrap();
        assert_eq!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn delete_orphans() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let _aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        repo.create(&Album::new("Orphan Album".into())).unwrap();
        let deleted = repo.delete_orphans().unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(repo.count().unwrap(), 0);
    }

    #[test]
    fn update_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();
        let mut album = Album::new("A Love Supreme".into());
        album.artist_id = Some(aid);
        album.year = Some(1965);
        let id = repo.create(&album).unwrap();

        album.id = Some(id);
        album.genre = Some("Jazz".into());
        album.label = Some("Impulse!".into());
        album.bio = Some("A masterpiece".into());
        repo.update(&album).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.genre.as_deref(), Some("Jazz"));
        assert_eq!(fetched.label.as_deref(), Some("Impulse!"));
        assert_eq!(fetched.bio.as_deref(), Some("A masterpiece"));
    }

    #[test]
    fn update_cover_path() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let id = repo.create(&Album::new("Test Album".into())).unwrap();
        repo.update_cover_path(id, "abc123").unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.cover_path.as_deref(), Some("abc123"));

        // COALESCE: does NOT overwrite existing cover_path
        repo.update_cover_path(id, "new_hash").unwrap();
        let fetched2 = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched2.cover_path.as_deref(), Some("abc123"));
    }

    #[test]
    fn force_update_cover_path() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let id = repo.create(&Album::new("Test Album".into())).unwrap();
        repo.update_cover_path(id, "abc123").unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.cover_path.as_deref(), Some("abc123"));

        // force: DOES overwrite existing cover_path (used by rescan endpoints)
        repo.force_update_cover_path(id, "new_hash").unwrap();
        let fetched2 = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched2.cover_path.as_deref(), Some("new_hash"));
    }

    #[test]
    fn list_albums() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Various".into())).unwrap();
        for title in ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"] {
            let mut a = Album::new(title.into());
            a.artist_id = Some(aid);
            repo.create(&a).unwrap();
        }

        let all = repo.list(100, 0).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].title, "Alpha");
        assert_eq!(all[4].title, "Gamma");

        let page = repo.list(2, 2).unwrap();
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn list_recent_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        for i in 0..5 {
            repo.create(&Album::new(format!("Album {i}"))).unwrap();
        }

        let recent = repo.list_recent(3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].title, "Album 4");
    }

    #[test]
    fn list_by_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let aid2 = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();

        let mut a1 = Album::new("Kind of Blue".into());
        a1.artist_id = Some(aid1);
        a1.year = Some(1959);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Bitches Brew".into());
        a2.artist_id = Some(aid1);
        a2.year = Some(1970);
        repo.create(&a2).unwrap();

        let mut a3 = Album::new("A Love Supreme".into());
        a3.artist_id = Some(aid2);
        repo.create(&a3).unwrap();

        let miles_albums = repo.list_by_artist(aid1).unwrap();
        assert_eq!(miles_albums.len(), 2);
        assert_eq!(miles_albums[0].title, "Kind of Blue");
        assert_eq!(miles_albums[1].title, "Bitches Brew");
    }

    #[test]
    fn list_by_genre() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut a1 = Album::new("Jazz Album".into());
        a1.genre = Some("Jazz".into());
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Rock Album".into());
        a2.genre = Some("Rock".into());
        repo.create(&a2).unwrap();

        let mut a3 = Album::new("Fusion Album".into());
        a3.genres = Some(r#"["Jazz","Fusion"]"#.into());
        repo.create(&a3).unwrap();

        let mut a4 = Album::new("Jazz Blues Album".into());
        a4.genre = Some("Jazz; Blues".into());
        repo.create(&a4).unwrap();

        let mut a5 = Album::new("Blues Rock Album".into());
        a5.genre = Some("Blues/Rock".into());
        repo.create(&a5).unwrap();

        let jazz = repo.list_by_genre("Jazz").unwrap();
        assert_eq!(jazz.len(), 3);

        let blues = repo.list_by_genre("Blues").unwrap();
        assert_eq!(blues.len(), 2);

        let rock = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock.len(), 2);

        let mut a6 = Album::new("Prog Album".into());
        a6.genre = Some("Progressive Rock".into());
        repo.create(&a6).unwrap();
        let rock2 = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock2.len(), 2);

        let prog = repo.list_by_genre("Progressive Rock").unwrap();
        assert_eq!(prog.len(), 1);
    }

    #[test]
    fn list_without_cover() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();

        let mut a1 = Album::new("No Cover".into());
        a1.artist_id = Some(aid);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Has Cover".into());
        a2.artist_id = Some(aid);
        a2.cover_path = Some("hash123".into());
        repo.create(&a2).unwrap();

        let without = repo.list_without_cover().unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].1, "No Cover");
    }

    #[test]
    fn search_albums() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let mut a = Album::new("The Dark Side of the Moon".into());
        a.artist_id = Some(aid);
        a.year = Some(1973);
        repo.create(&a).unwrap();

        let results = repo.search("dark", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "The Dark Side of the Moon");
    }

    #[test]
    fn get_by_musicbrainz_release_id() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut album = Album::new("Test".into());
        album.musicbrainz_release_id = Some("12345-abcde".into());
        let id = repo.create(&album).unwrap();

        let found = repo.get_by_musicbrainz_release_id("12345-abcde").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, Some(id));

        assert!(
            repo.get_by_musicbrainz_release_id("nonexistent")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn count_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        repo.create(&Album::new("A".into())).unwrap();
        repo.create(&Album::new("B".into())).unwrap();
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn album_quality_classification() {
        let mut a = Album::new("DSD Album".into());
        a.format = Some("dsf".into());
        assert_eq!(a.quality(), Some("dsd".into()));

        let mut b = Album::new("Hi-Res Album".into());
        b.sample_rate = Some(96000);
        b.bit_depth = Some(24);
        b.format = Some("flac".into());
        assert_eq!(b.quality(), Some("hi-res".into()));

        let mut c = Album::new("CD Album".into());
        c.format = Some("flac".into());
        c.sample_rate = Some(44100);
        c.bit_depth = Some(16);
        assert_eq!(c.quality(), Some("cd".into()));

        let mut d = Album::new("Lossy Album".into());
        d.format = Some("mp3".into());
        assert_eq!(d.quality(), Some("lossy".into()));

        let e = Album::new("Unknown".into());
        assert_eq!(e.quality(), None);
    }

    #[test]
    fn album_to_json() {
        let mut a = Album::new("Test".into());
        a.format = Some("flac".into());
        a.sample_rate = Some(96000);
        a.bit_depth = Some(24);
        let json = a.to_json();
        assert_eq!(json["quality"], "hi-res");
        assert_eq!(json["title"], "Test");
    }

    #[test]
    fn get_or_create_without_year() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        let a1 = repo.get_or_create("Album", aid, None).unwrap();
        let a2 = repo.get_or_create("Album", aid, None).unwrap();
        assert_eq!(a1.id, a2.id);
    }

    #[test]
    fn get_or_create_same_title_different_artists() {
        // Regression test: "One by One" by Grey Reverend must NOT be merged
        // with "One by One" by Robert Francis.
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Grey Reverend".into()))
            .unwrap();
        let aid2 = artist_repo
            .create(&Artist::new("Robert Francis".into()))
            .unwrap();

        let a1 = repo.get_or_create("One by One", aid1, Some(2010)).unwrap();
        let a2 = repo.get_or_create("One by One", aid2, Some(2013)).unwrap();

        // Must be two different albums
        assert_ne!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 2);

        // Same artist + same title => same album
        let a3 = repo.get_or_create("One by One", aid1, Some(2010)).unwrap();
        assert_eq!(a1.id, a3.id);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn get_or_create_with_mbid_same_title_different_artists() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Grey Reverend".into()))
            .unwrap();
        let aid2 = artist_repo
            .create(&Artist::new("Robert Francis".into()))
            .unwrap();

        let a1 = repo
            .get_or_create_with_mbid("One by One", aid1, Some(2010), None)
            .unwrap();
        let a2 = repo
            .get_or_create_with_mbid("One by One", aid2, Some(2013), None)
            .unwrap();

        assert_ne!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn update_quality_from_tracks() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        let alid = album_repo
            .get_or_create("Test Album", aid, None)
            .unwrap()
            .id
            .unwrap();

        let mut t = crate::db::models::Track::new("Track 1".into());
        t.album_id = Some(alid);
        t.format = Some("flac".into());
        t.sample_rate = Some(96000);
        t.bit_depth = Some(24);
        t.file_path = Some("/test.flac".into());
        track_repo.create(&t).unwrap();

        album_repo.update_quality_from_tracks(alid).unwrap();
        let album = album_repo.get(alid).unwrap().unwrap();
        assert_eq!(album.format.as_deref(), Some("flac"));
        assert_eq!(album.sample_rate, Some(96000));
        assert_eq!(album.bit_depth, Some(24));
    }

    #[test]
    fn update_track_count() {
        let db = test_db();
        let album_repo = AlbumRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db);

        let alid = album_repo.create(&Album::new("Test".into())).unwrap();
        let mut t1 = crate::db::models::Track::new("A".into());
        t1.album_id = Some(alid);
        t1.file_path = Some("/a.flac".into());
        let mut t2 = crate::db::models::Track::new("B".into());
        t2.album_id = Some(alid);
        t2.file_path = Some("/b.flac".into());
        track_repo.create(&t1).unwrap();
        track_repo.create(&t2).unwrap();

        album_repo.update_track_count(alid).unwrap();
        let album = album_repo.get(alid).unwrap().unwrap();
        assert_eq!(album.track_count, Some(2));
    }

    #[test]
    fn unicode_album_title() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut a = Album::new("Concerto pour clarinette en la majeur".into());
        a.genre = Some("Classique".into());
        let id = repo.create(&a).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "Concerto pour clarinette en la majeur");
    }

    #[test]
    fn delete_nonexistent_album() {
        let db = test_db();
        let repo = AlbumRepo::new(db);
        repo.delete(999).unwrap();
    }

    #[test]
    fn list_sorted_by_title() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Various".into())).unwrap();
        for title in ["Gamma", "Alpha", "Beta"] {
            let mut a = Album::new(title.into());
            a.artist_id = Some(aid);
            repo.create(&a).unwrap();
        }

        let asc = repo.list_sorted(100, 0, "title", "asc").unwrap();
        assert_eq!(asc[0].title, "Alpha");
        assert_eq!(asc[2].title, "Gamma");

        let desc = repo.list_sorted(100, 0, "title", "desc").unwrap();
        assert_eq!(desc[0].title, "Gamma");
        assert_eq!(desc[2].title, "Alpha");
    }

    #[test]
    fn list_sorted_by_year() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Artist".into())).unwrap();

        let mut a1 = Album::new("Old".into());
        a1.artist_id = Some(aid);
        a1.year = Some(1970);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("New".into());
        a2.artist_id = Some(aid);
        a2.year = Some(2020);
        repo.create(&a2).unwrap();

        let asc = repo.list_sorted(100, 0, "year", "asc").unwrap();
        assert_eq!(asc[0].title, "Old");
        assert_eq!(asc[1].title, "New");

        let desc = repo.list_sorted(100, 0, "release_date", "desc").unwrap();
        assert_eq!(desc[0].title, "New");
        assert_eq!(desc[1].title, "Old");
    }

    #[test]
    fn list_sorted_by_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid_z = artist_repo.create(&Artist::new("Zappa".into())).unwrap();
        let aid_a = artist_repo.create(&Artist::new("Abba".into())).unwrap();

        let mut a1 = Album::new("Hot Rats".into());
        a1.artist_id = Some(aid_z);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Arrival".into());
        a2.artist_id = Some(aid_a);
        repo.create(&a2).unwrap();

        let asc = repo.list_sorted(100, 0, "artist", "asc").unwrap();
        assert_eq!(asc[0].title, "Arrival");
        assert_eq!(asc[1].title, "Hot Rats");

        let desc = repo.list_sorted(100, 0, "artist", "desc").unwrap();
        assert_eq!(desc[0].title, "Hot Rats");
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::get_by_id(&s).ends_with("WHERE a.id = ?"));
        assert!(sql::get_by_id(&p).ends_with("WHERE a.id = $1"));
        assert!(sql::get_by_title(&p).contains("LOWER(a.title) = LOWER($1)"));
        assert!(sql::create_minimal(&p).contains("VALUES ($1, $2, $3)"));
        assert!(!sql::list_by_artist(&p).contains("COLLATE"));
    }

    #[test]
    fn search_uses_engine_specific_fts_clause() {
        let s_sql = sql::search(&SqliteDialect);
        assert!(s_sql.contains("a.id IN (SELECT rowid FROM albums_fts WHERE albums_fts MATCH ?)"));
        let p_sql = sql::search(&PostgresDialect);
        assert!(p_sql.contains("a.search_tsv @@ to_tsquery('simple', unaccent($1))"));
    }

    #[test]
    fn list_sorted_by_added_at() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        repo.create(&Album::new("First".into())).unwrap();
        repo.create(&Album::new("Second".into())).unwrap();
        repo.create(&Album::new("Third".into())).unwrap();

        let asc = repo.list_sorted(100, 0, "added_at", "asc").unwrap();
        assert_eq!(asc[0].title, "First");
        assert_eq!(asc[2].title, "Third");

        let desc = repo.list_sorted(100, 0, "added_at", "desc").unwrap();
        assert_eq!(desc[0].title, "Third");
        assert_eq!(desc[2].title, "First");
    }

    #[test]
    fn with_backend_constructor_full() {
        // All methods now go through DbBackend — no more sqlite_legacy.
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let aid = artist_repo.create(&Artist::new("X".into())).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = AlbumRepo::with_backend(backend);
        let id = repo.create(&Album::new("Album X".into())).unwrap();
        assert!(repo.get(id).unwrap().is_some());
        // Previously-legacy methods now work via DbBackend.
        let a = repo.get_or_create("Created", aid, Some(2024)).unwrap();
        assert!(a.id.is_some());
        // Idempotent — second call returns the same row.
        let a2 = repo.get_or_create("Created", aid, Some(2024)).unwrap();
        assert_eq!(a.id, a2.id);
        // list_by_genre returns an empty list rather than erroring.
        assert!(repo.list_by_genre("Jazz").unwrap().is_empty());
    }
}
