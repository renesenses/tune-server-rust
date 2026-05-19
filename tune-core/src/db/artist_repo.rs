use rusqlite::params;

use super::models::Artist;
use super::sqlite::SqliteDb;

pub struct ArtistRepo {
    db: SqliteDb,
}

impl ArtistRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Artist>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source FROM artists WHERE id = ?")
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![id], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn get_by_name(&self, name: &str) -> Result<Option<Artist>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source FROM artists WHERE name = ? COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![name], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn get_by_musicbrainz_id(&self, mbid: &str) -> Result<Option<Artist>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source FROM artists WHERE musicbrainz_id = ?")
            .map_err(|e| e.to_string())?;
        let result = stmt
            .query_row(params![mbid], |row| Ok(row_to_artist(row)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    pub fn create(&self, artist: &Artist) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO artists (name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source) VALUES (?, ?, ?, ?, ?, ?, ?)",
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

    pub fn get_or_create(&self, name: &str, musicbrainz_id: Option<&str>, sort_name: Option<&str>) -> Result<Artist, String> {
        if let Some(mbid) = musicbrainz_id {
            if let Some(artist) = self.get_by_musicbrainz_id(mbid)? {
                return Ok(artist);
            }
        }
        if let Some(artist) = self.get_by_name(name)? {
            return Ok(artist);
        }
        let mut artist = Artist::new(name.to_string());
        artist.sort_name = sort_name.map(|s| s.to_string());
        artist.musicbrainz_id = musicbrainz_id.map(|s| s.to_string());
        let id = self.create(&artist)?;
        artist.id = Some(id);
        Ok(artist)
    }

    pub fn update(&self, artist: &Artist) -> Result<(), String> {
        let id = artist.id.ok_or("artist has no id")?;
        self.db.execute(
            "UPDATE artists SET name = ?, sort_name = ?, musicbrainz_id = ?, discogs_id = ?, bio = ?, image_path = ?, image_source = ? WHERE id = ?",
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
        self.db.execute("DELETE FROM artists WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM artists", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Artist>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source FROM artists ORDER BY COALESCE(sort_name, name) COLLATE NOCASE LIMIT ? OFFSET ?")
            .map_err(|e| e.to_string())?;
        let artists = stmt
            .query_map(params![limit, offset], |row| Ok(row_to_artist(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(artists)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Artist>, String> {
        let like = format!("%{query}%");
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, sort_name, musicbrainz_id, discogs_id, bio, image_path, image_source FROM artists WHERE name LIKE ? COLLATE NOCASE LIMIT ?")
            .map_err(|e| e.to_string())?;
        let artists = stmt
            .query_map(params![like, limit], |row| Ok(row_to_artist(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
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
        let repo = ArtistRepo::new(db);

        let a1 = repo.get_or_create("Beatles", None, None).unwrap();
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
}
