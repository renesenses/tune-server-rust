use rusqlite::{OptionalExtension, params};

use super::models::Album;
use super::sqlite::SqliteDb;

const SELECT_ALBUM: &str = "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.original_year, a.genre, a.disc_count, a.track_count, a.cover_path, a.source, a.source_id, a.label, a.catalog_number, a.barcode, a.format, a.sample_rate, a.bit_depth, a.bio, a.musicbrainz_release_id, a.musicbrainz_release_group_id, a.release_date, a.original_date, a.genres FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id";

pub struct AlbumRepo {
    db: SqliteDb,
}

impl AlbumRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.id = ?"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_title(&self, title: &str) -> Result<Option<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.title = ? COLLATE NOCASE LIMIT 1"
            ))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![title], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_title_and_artist(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Option<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        // Try exact match with year first
        if let Some(y) = year {
            let mut stmt = conn
                .prepare(&format!(
                    "{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ? AND a.year = ?"
                ))
                .map_err(|e| e.to_string())?;
            if let Some(album) = stmt
                .query_row(params![title, artist_id, y], |row| Ok(row_to_album(row)))
                .optional()
                .map_err(|e| e.to_string())?
            {
                return Ok(Some(album));
            }
        }
        // Fallback: match by title + artist without year (avoids duplicate albums
        // when tracks from the same album have different or missing year tags)
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ?"
            ))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![title, artist_id], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_title_only(&self, title: &str) -> Result<Option<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.title = ? LIMIT 1"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![title], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_musicbrainz_release_id(&self, release_id: &str) -> Result<Option<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.musicbrainz_release_id = ?"
            ))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![release_id], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn create(&self, album: &Album) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO albums (title, artist_id, year, original_year, genre, genres, disc_count, track_count, cover_path, source, source_id, label, catalog_number, barcode, format, sample_rate, bit_depth, bio, musicbrainz_release_id, musicbrainz_release_group_id, release_date, original_date) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &album.title as &dyn rusqlite::types::ToSql,
                &album.artist_id, &album.year, &album.original_year,
                &album.genre, &album.genres, &album.disc_count, &album.track_count,
                &album.cover_path, &album.source, &album.source_id,
                &album.label, &album.catalog_number, &album.barcode,
                &album.format, &album.sample_rate, &album.bit_depth,
                &album.bio, &album.musicbrainz_release_id,
                &album.musicbrainz_release_group_id,
                &album.release_date, &album.original_date,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Look up or create an album, using the write connection for the entire
    /// read-then-write sequence. This ensures that during a scan transaction
    /// (BEGIN IMMEDIATE), the lookup sees albums created earlier in the same
    /// batch, preventing duplicates.
    pub fn get_or_create(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Album, String> {
        self.db.write(|conn| {
            if let Some(album) = lookup_album(conn, title, artist_id, year)? {
                return Ok(album);
            }
            conn.execute(
                "INSERT INTO albums (title, artist_id, year) VALUES (?, ?, ?)",
                rusqlite::params![title, artist_id, year],
            )?;
            let id = conn.last_insert_rowid();
            let mut album = Album::new(title.to_string());
            album.id = Some(id);
            album.artist_id = Some(artist_id);
            album.year = year;
            Ok(album)
        })
    }

    /// Like `get_or_create` but also checks MusicBrainz release ID first.
    pub fn get_or_create_with_mbid(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        mbid: Option<&str>,
    ) -> Result<Album, String> {
        self.db.write(|conn| {
            if let Some(release_id) = mbid {
                let mut stmt = conn.prepare_cached(
                    &format!("{SELECT_ALBUM} WHERE a.musicbrainz_release_id = ?"),
                )?;
                if let Some(album) = stmt
                    .query_row(params![release_id], |row| Ok(row_to_album(row)))
                    .optional()?
                {
                    return Ok(album);
                }
            }
            if let Some(album) = lookup_album(conn, title, artist_id, year)? {
                return Ok(album);
            }
            conn.execute(
                "INSERT INTO albums (title, artist_id, year, musicbrainz_release_id) VALUES (?, ?, ?, ?)",
                rusqlite::params![title, artist_id, year, mbid],
            )?;
            let id = conn.last_insert_rowid();
            let mut album = Album::new(title.to_string());
            album.id = Some(id);
            album.artist_id = Some(artist_id);
            album.year = year;
            album.musicbrainz_release_id = mbid.map(String::from);
            Ok(album)
        })
    }

    pub fn update(&self, album: &Album) -> Result<(), String> {
        let id = album.id.ok_or("album has no id")?;
        self.db.execute(
            "UPDATE albums SET title = ?, artist_id = ?, year = ?, original_year = ?, genre = ?, genres = ?, disc_count = ?, track_count = ?, cover_path = ?, label = ?, catalog_number = ?, format = ?, sample_rate = ?, bit_depth = ?, bio = ?, musicbrainz_release_id = ?, musicbrainz_release_group_id = ? WHERE id = ?",
            &[
                &album.title as &dyn rusqlite::types::ToSql,
                &album.artist_id, &album.year, &album.original_year,
                &album.genre, &album.genres, &album.disc_count, &album.track_count,
                &album.cover_path, &album.label, &album.catalog_number,
                &album.format, &album.sample_rate, &album.bit_depth,
                &album.bio, &album.musicbrainz_release_id,
                &album.musicbrainz_release_group_id, &id,
            ],
        )?;
        Ok(())
    }

    pub fn update_cover_path(&self, album_id: i64, cover_path: &str) -> Result<(), String> {
        self.db.execute(
            "UPDATE albums SET cover_path = ? WHERE id = ? AND (cover_path IS NULL OR cover_path = '')",
            &[&cover_path as &dyn rusqlite::types::ToSql, &album_id],
        )?;
        Ok(())
    }

    pub fn update_track_count(&self, album_id: i64) -> Result<(), String> {
        self.db.execute(
            "UPDATE albums SET track_count = (SELECT COUNT(*) FROM tracks WHERE album_id = ?) WHERE id = ?",
            &[&album_id, &album_id],
        )?;
        Ok(())
    }

    pub fn update_quality_from_tracks(&self, album_id: i64) -> Result<(), String> {
        self.db.execute(
            "UPDATE albums SET
                format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = ? AND t.format IS NOT NULL LIMIT 1)),
                sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = ?)),
                bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = ?)),
                genre = COALESCE(albums.genre, (SELECT t.genre FROM tracks t WHERE t.album_id = ? AND t.genre IS NOT NULL LIMIT 1)),
                genres = COALESCE(albums.genres, (SELECT t.genres FROM tracks t WHERE t.album_id = ? AND t.genres IS NOT NULL LIMIT 1)),
                disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = ?))
            WHERE id = ?",
            &[&album_id, &album_id, &album_id, &album_id, &album_id, &album_id, &album_id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM albums WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn delete_orphans(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if count > 0 {
            conn.execute(
                "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
                [],
            ).map_err(|e| e.to_string())?;
        }
        Ok(count)
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} ORDER BY a.id DESC LIMIT ?"))
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn list_by_release_group(&self, group_id: &str) -> Result<Vec<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.musicbrainz_release_group_id = ? ORDER BY a.year, a.title"
            ))
            .map_err(|e| e.to_string())?;
        stmt.query_map(params![group_id], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn list_release_groups(&self) -> Result<Vec<(String, Vec<Album>)>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.musicbrainz_release_group_id IS NOT NULL \
                 AND a.musicbrainz_release_group_id != '' \
                 ORDER BY a.musicbrainz_release_group_id, a.year"
            ))
            .map_err(|e| e.to_string())?;
        let albums: Vec<Album> = stmt
            .query_map([], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        drop(conn);

        let mut groups: std::collections::HashMap<String, Vec<Album>> =
            std::collections::HashMap::new();
        for album in albums {
            if let Some(ref gid) = album.musicbrainz_release_group_id {
                groups.entry(gid.clone()).or_default().push(album);
            }
        }
        Ok(groups.into_iter().filter(|(_, v)| v.len() > 1).collect())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Album>, String> {
        self.list_sorted(limit, offset, "title", "asc")
    }

    pub fn list_sorted(
        &self,
        limit: i64,
        offset: i64,
        sort: &str,
        order: &str,
    ) -> Result<Vec<Album>, String> {
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
    ) -> Result<Vec<Album>, String> {
        let dir = if order.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            "ASC"
        };
        let order_clause = match sort {
            "title" => format!("a.title COLLATE NOCASE {dir}"),
            "release_date" | "year" => format!("a.year {dir}, a.title COLLATE NOCASE ASC"),
            "artist" => {
                format!("ar.name COLLATE NOCASE {dir}, a.year ASC, a.title COLLATE NOCASE ASC")
            }
            _ => format!("a.id {dir}"),
        };
        let mut wheres = Vec::new();
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(fmt) = format {
            wheres.push(
                "a.id IN (SELECT DISTINCT album_id FROM tracks WHERE format = ?)".to_string(),
            );
            bind_values.push(Box::new(fmt.to_string()));
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

        let sql = format!("{SELECT_ALBUM}{where_clause} ORDER BY {order_clause} LIMIT ? OFFSET ?");
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;

        let mut params_vec: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();
        params_vec.push(&limit);
        params_vec.push(&offset);

        let albums = stmt
            .query_map(params_vec.as_slice(), |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(albums)
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.artist_id = ? ORDER BY a.year, a.title"
            ))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![artist_id], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(albums)
    }

    pub fn list_by_genre(&self, genre: &str) -> Result<Vec<Album>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        // Match albums where the requested genre appears in either:
        // 1) The legacy `genre` text column (may contain "Jazz; Blues" or "Jazz/Blues")
        //    We normalize separators to commas and use delimiter-aware LIKE matching
        //    so "Rock" does not accidentally match "Progressive Rock".
        // 2) The structured `genres` JSON array via json_each() for exact element match.
        let delimited_pattern = format!("%,{},%", genre.replace('%', "").replace('_', ""));
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE \
                 (',' || REPLACE(REPLACE(REPLACE(REPLACE(a.genre, '; ', ','), ';', ','), '/ ', ','), '/', ',') || ',') LIKE ? COLLATE NOCASE \
                 OR a.id IN (SELECT a2.id FROM albums a2, json_each(a2.genres) WHERE json_each.value = ? COLLATE NOCASE) \
                 ORDER BY a.title COLLATE NOCASE"
            ))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![delimited_pattern, genre], |row| {
                Ok(row_to_album(row))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(albums)
    }

    /// Return all local albums that have no cover art set.
    /// Each entry is (album_id, title, artist_name, musicbrainz_release_id).
    #[allow(clippy::type_complexity)]
    pub fn list_without_cover(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>, Option<String>)>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT a.id, a.title, ar.name, a.musicbrainz_release_id \
                 FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                 WHERE (a.cover_path IS NULL OR a.cover_path = '') \
                 AND a.source = 'local' \
                 ORDER BY a.id",
            )
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Album>, String> {
        let fts_query = format!("{query}*");
        let like = format!("%{query}%");
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.id IN (SELECT rowid FROM albums_fts WHERE albums_fts MATCH ?) OR a.title LIKE ? COLLATE NOCASE OR ar.name LIKE ? COLLATE NOCASE OR a.genre LIKE ? COLLATE NOCASE OR CAST(a.year AS TEXT) = ? OR a.label LIKE ? COLLATE NOCASE LIMIT ?"))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(
                params![fts_query, like, like, like, query.trim(), like, limit],
                |row| Ok(row_to_album(row)),
            )
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(albums)
    }
}

/// Shared lookup logic used by `get_or_create` and `get_or_create_with_mbid`.
/// Runs on an already-locked write connection to see uncommitted rows.
fn lookup_album(
    conn: &rusqlite::Connection,
    title: &str,
    artist_id: i64,
    year: Option<i32>,
) -> Result<Option<Album>, rusqlite::Error> {
    if let Some(y) = year {
        let mut stmt = conn.prepare_cached(&format!(
            "{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ? AND a.year = ?"
        ))?;
        if let Some(album) = stmt
            .query_row(params![title, artist_id, y], |row| Ok(row_to_album(row)))
            .optional()?
        {
            return Ok(Some(album));
        }
    }
    let mut stmt = conn.prepare_cached(&format!(
        "{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ?"
    ))?;
    stmt.query_row(params![title, artist_id], |row| Ok(row_to_album(row)))
        .optional()
}

fn row_to_album(row: &rusqlite::Row) -> Album {
    Album {
        id: row.get(0).ok(),
        title: row.get(1).unwrap_or_default(),
        artist_id: row.get(2).ok(),
        artist_name: row.get(3).ok(),
        year: row.get(4).ok(),
        original_year: row.get(5).ok(),
        genre: row.get(6).ok(),
        genres: row.get::<_, Option<String>>(23).ok().flatten(),
        disc_count: row.get(7).ok(),
        track_count: row.get(8).ok(),
        cover_path: row.get(9).ok(),
        source: row.get(10).unwrap_or_else(|_| "local".into()),
        source_id: row.get(11).ok(),
        label: row.get(12).ok(),
        catalog_number: row.get(13).ok(),
        barcode: row.get(14).ok(),
        format: row.get(15).ok(),
        sample_rate: row.get(16).ok(),
        bit_depth: row.get(17).ok(),
        bio: row.get(18).ok(),
        musicbrainz_release_id: row.get(19).ok(),
        musicbrainz_release_group_id: row.get(20).ok(),
        release_date: row.get(21).ok(),
        original_date: row.get(22).ok(),
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

        // Should not overwrite existing cover
        repo.update_cover_path(id, "new_hash").unwrap();
        let fetched2 = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched2.cover_path.as_deref(), Some("abc123"));
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
        // Should be sorted by title (COLLATE NOCASE)
        assert_eq!(all[0].title, "Alpha");
        assert_eq!(all[4].title, "Gamma"); // G after E

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
        // Most recent first (highest id)
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
        // Should be sorted by year
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

        // Semicolon-separated multi-genre in legacy column
        let mut a4 = Album::new("Jazz Blues Album".into());
        a4.genre = Some("Jazz; Blues".into());
        repo.create(&a4).unwrap();

        // Slash-separated multi-genre in legacy column
        let mut a5 = Album::new("Blues Rock Album".into());
        a5.genre = Some("Blues/Rock".into());
        repo.create(&a5).unwrap();

        // Jazz: a1 (exact), a3 (JSON array), a4 (semicolon-separated)
        let jazz = repo.list_by_genre("Jazz").unwrap();
        assert_eq!(
            jazz.len(),
            3,
            "Jazz should match exact, JSON, and semicolon-separated"
        );

        // Blues: a4 (semicolon-separated), a5 (slash-separated)
        let blues = repo.list_by_genre("Blues").unwrap();
        assert_eq!(
            blues.len(),
            2,
            "Blues should match semicolon and slash-separated"
        );

        // Rock: a2 (exact), a5 (slash-separated)
        let rock = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock.len(), 2, "Rock should match exact and slash-separated");

        // "Progressive Rock" should NOT match plain "Rock"
        let mut a6 = Album::new("Prog Album".into());
        a6.genre = Some("Progressive Rock".into());
        repo.create(&a6).unwrap();
        let rock2 = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock2.len(), 2, "Rock should not match Progressive Rock");

        // But "Progressive Rock" should match itself
        let prog = repo.list_by_genre("Progressive Rock").unwrap();
        assert_eq!(prog.len(), 1, "Progressive Rock should match exactly");
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
        // Should not error
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
        assert_eq!(asc[0].title, "Arrival"); // Abba first
        assert_eq!(asc[1].title, "Hot Rats"); // Zappa second

        let desc = repo.list_sorted(100, 0, "artist", "desc").unwrap();
        assert_eq!(desc[0].title, "Hot Rats"); // Zappa first
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
}
