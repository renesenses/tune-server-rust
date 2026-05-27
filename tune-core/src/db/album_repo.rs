use rusqlite::{params, OptionalExtension};

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
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.id = ?"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_album(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_title_and_artist(&self, title: &str, artist_id: i64, year: Option<i32>) -> Result<Option<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        if let Some(y) = year {
            let mut stmt = conn
                .prepare(&format!("{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ? AND a.year = ?"))
                .map_err(|e| e.to_string())?;
            stmt.query_row(params![title, artist_id, y], |row| Ok(row_to_album(row)))
                .optional()
                .map_err(|e| e.to_string())
        } else {
            let mut stmt = conn
                .prepare(&format!("{SELECT_ALBUM} WHERE a.title = ? AND a.artist_id = ?"))
                .map_err(|e| e.to_string())?;
            stmt.query_row(params![title, artist_id], |row| Ok(row_to_album(row)))
                .optional()
                .map_err(|e| e.to_string())
        }
    }

    pub fn get_by_musicbrainz_release_id(&self, release_id: &str) -> Result<Option<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.musicbrainz_release_id = ?"))
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

    pub fn get_or_create(&self, title: &str, artist_id: i64, year: Option<i32>) -> Result<Album, String> {
        if let Some(album) = self.get_by_title_and_artist(title, artist_id, year)? {
            return Ok(album);
        }
        let mut album = Album::new(title.to_string());
        album.artist_id = Some(artist_id);
        album.year = year;
        let id = self.create(&album)?;
        album.id = Some(id);
        Ok(album)
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
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} ORDER BY a.id DESC LIMIT ?"))
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} ORDER BY a.title COLLATE NOCASE LIMIT ? OFFSET ?"))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![limit, offset], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(albums)
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.artist_id = ? ORDER BY a.year, a.title"))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![artist_id], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(albums)
    }

    pub fn list_by_genre(&self, genre: &str) -> Result<Vec<Album>, String> {
        let conn = self.db.connection().lock().unwrap();
        // Match against primary genre OR any genre in the JSON genres array.
        // The JSON pattern matches `"Jazz"` as an element inside `["Jazz","Fusion"]`.
        let json_pattern = format!("%\"{}%", genre.replace('"', ""));
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_ALBUM} WHERE a.genre = ? OR a.genres LIKE ? COLLATE NOCASE ORDER BY a.title COLLATE NOCASE"
            ))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![genre, json_pattern], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(albums)
    }

    /// Return all local albums that have no cover art set.
    /// Each entry is (album_id, title, artist_name, musicbrainz_release_id).
    pub fn list_without_cover(&self) -> Result<Vec<(i64, String, Option<String>, Option<String>)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT a.id, a.title, ar.name, a.musicbrainz_release_id \
                 FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                 WHERE (a.cover_path IS NULL OR a.cover_path = '') \
                 AND a.source = 'local' \
                 ORDER BY a.id"
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
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Album>, String> {
        let fts_query = format!("{query}*");
        let like = format!("%{query}%");
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_ALBUM} WHERE a.id IN (SELECT rowid FROM albums_fts WHERE albums_fts MATCH ?) OR a.title LIKE ? COLLATE NOCASE OR ar.name LIKE ? COLLATE NOCASE OR a.genre LIKE ? COLLATE NOCASE OR CAST(a.year AS TEXT) = ? OR a.label LIKE ? COLLATE NOCASE LIMIT ?"))
            .map_err(|e| e.to_string())?;
        let albums = stmt
            .query_map(params![fts_query, like, like, like, query.trim(), like, limit], |row| Ok(row_to_album(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(albums)
    }
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

        let artist_id = artist_repo.create(&Artist::new("Pink Floyd".into())).unwrap();
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
        let a1 = repo.get_or_create("Abbey Road", artist_id, Some(1969)).unwrap();
        let a2 = repo.get_or_create("Abbey Road", artist_id, Some(1969)).unwrap();
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
}
