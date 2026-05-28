use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};

use super::models::Track;
use super::sqlite::SqliteDb;

const SELECT_TRACK: &str = "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, t.album_artist, t.disc_number, t.disc_subtitle, t.track_number, t.duration_ms, t.file_path, t.format, t.sample_rate, t.bit_depth, t.channels, t.file_mtime, t.file_size, t.audio_hash, t.source, t.source_id, t.isrc, t.genre, t.composer, t.year, t.bpm, t.label, t.musicbrainz_recording_id, al.cover_path, t.genres FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id";

pub struct TrackRepo {
    db: SqliteDb,
}

impl TrackRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Track>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_TRACK} WHERE t.id = ?"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_track(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_by_path(&self, file_path: &str) -> Result<Option<Track>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_TRACK} WHERE t.file_path = ?"))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![file_path], |row| Ok(row_to_track(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn create(&self, track: &Track) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO tracks (title, album_id, artist_id, album_artist, disc_number, disc_subtitle, track_number, duration_ms, file_path, format, sample_rate, bit_depth, channels, file_mtime, file_size, audio_hash, source, source_id, isrc, genre, genres, composer, year, bpm, label, musicbrainz_recording_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &track.title as &dyn rusqlite::types::ToSql,
                &track.album_id, &track.artist_id, &track.album_artist,
                &track.disc_number, &track.disc_subtitle,
                &track.track_number, &track.duration_ms,
                &track.file_path, &track.format,
                &track.sample_rate, &track.bit_depth, &track.channels,
                &track.file_mtime, &track.file_size, &track.audio_hash,
                &track.source, &track.source_id, &track.isrc,
                &track.genre, &track.genres, &track.composer, &track.year,
                &track.bpm, &track.label, &track.musicbrainz_recording_id,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn update(&self, track: &Track) -> Result<(), String> {
        let id = track.id.ok_or("track has no id")?;
        self.db.execute(
            "UPDATE tracks SET title = ?, album_id = ?, artist_id = ?, album_artist = ?, disc_number = ?, disc_subtitle = ?, track_number = ?, duration_ms = ?, file_path = ?, format = ?, sample_rate = ?, bit_depth = ?, channels = ?, file_mtime = ?, file_size = ?, audio_hash = ?, genre = ?, genres = ?, composer = ?, year = ?, bpm = ?, label = ?, musicbrainz_recording_id = ? WHERE id = ?",
            &[
                &track.title as &dyn rusqlite::types::ToSql,
                &track.album_id, &track.artist_id, &track.album_artist,
                &track.disc_number, &track.disc_subtitle,
                &track.track_number, &track.duration_ms,
                &track.file_path, &track.format,
                &track.sample_rate, &track.bit_depth, &track.channels,
                &track.file_mtime, &track.file_size, &track.audio_hash,
                &track.genre, &track.genres, &track.composer, &track.year,
                &track.bpm, &track.label, &track.musicbrainz_recording_id,
                &id,
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM tracks WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn delete_by_path(&self, file_path: &str) -> Result<(), String> {
        self.db.execute(
            "DELETE FROM tracks WHERE file_path = ?",
            &[&file_path as &dyn rusqlite::types::ToSql],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Track>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_TRACK} ORDER BY ar.name COLLATE NOCASE, al.title COLLATE NOCASE, t.disc_number, t.track_number LIMIT ? OFFSET ?"))
            .map_err(|e| e.to_string())?;
        let tracks = stmt
            .query_map(params![limit, offset], |row| Ok(row_to_track(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tracks)
    }

    pub fn get_all_paths(&self) -> Result<HashSet<String>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT file_path FROM tracks WHERE source = 'local' AND file_path IS NOT NULL",
            )
            .map_err(|e| e.to_string())?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    /// Returns a map of file_path -> (track_id, file_mtime, file_size) for all local tracks.
    /// Used by the scanner to efficiently detect which files have changed without per-file queries.
    pub fn get_all_local_file_info(
        &self,
    ) -> Result<HashMap<String, (i64, Option<f64>, Option<i64>)>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, file_path, file_mtime, file_size FROM tracks WHERE source = 'local' AND file_path IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let map = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    (
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<f64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(map)
    }

    pub fn update_mtime_and_size(
        &self,
        file_path: &str,
        mtime: f64,
        file_size: i64,
    ) -> Result<(), String> {
        self.db.execute(
            "UPDATE tracks SET file_mtime = ?, file_size = ? WHERE file_path = ?",
            &[
                &mtime as &dyn rusqlite::types::ToSql,
                &file_size,
                &file_path,
            ],
        )?;
        Ok(())
    }

    pub fn update_audio_hash(&self, file_path: &str, audio_hash: &str) -> Result<(), String> {
        self.db.execute(
            "UPDATE tracks SET audio_hash = ? WHERE file_path = ?",
            &[&audio_hash as &dyn rusqlite::types::ToSql, &file_path],
        )?;
        Ok(())
    }

    pub fn list_by_album(&self, album_id: i64) -> Result<Vec<Track>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("{SELECT_TRACK} WHERE t.album_id = ? ORDER BY t.disc_number, t.track_number, t.file_path"))
            .map_err(|e| e.to_string())?;
        let tracks = stmt
            .query_map(params![album_id], |row| Ok(row_to_track(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tracks)
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Track>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_TRACK} WHERE t.artist_id = ? ORDER BY t.title"
            ))
            .map_err(|e| e.to_string())?;
        let tracks = stmt
            .query_map(params![artist_id], |row| Ok(row_to_track(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tracks)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Track>, String> {
        let fts_query = format!("{query}*");
        let like = format!("%{query}%");
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_TRACK} WHERE t.id IN (SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ?) OR ar.name LIKE ? COLLATE NOCASE OR t.genre LIKE ? COLLATE NOCASE OR t.composer LIKE ? COLLATE NOCASE OR CAST(al.year AS TEXT) = ? LIMIT ?"
            ))
            .map_err(|e| e.to_string())?;
        let tracks = stmt
            .query_map(
                params![fts_query, like, like, like, query.trim(), limit],
                |row| Ok(row_to_track(row)),
            )
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tracks)
    }

    pub fn get_multiple(&self, ids: &[i64]) -> Result<Vec<Track>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!("{SELECT_TRACK} WHERE t.id IN ({})", placeholders.join(","));
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let params: Vec<&dyn rusqlite::types::ToSql> = ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        let tracks: Vec<Track> = stmt
            .query_map(params.as_slice(), |row| Ok(row_to_track(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        // Preserve caller's ordering
        let mut ordered = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = tracks.iter().find(|t| t.id == Some(*id)) {
                ordered.push(t.clone());
            }
        }
        Ok(ordered)
    }

    pub fn random_ids(&self, limit: i64) -> Result<Vec<i64>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM tracks ORDER BY RANDOM() LIMIT ?")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map(rusqlite::params![limit], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    pub fn deduplicate(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracks t1 WHERE EXISTS (SELECT 1 FROM tracks t2 WHERE t2.audio_hash = t1.audio_hash AND t2.id < t1.id AND t1.audio_hash IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;

        if count > 0 {
            conn.execute(
                "DELETE FROM tracks WHERE id IN (SELECT t1.id FROM tracks t1 WHERE EXISTS (SELECT 1 FROM tracks t2 WHERE t2.audio_hash = t1.audio_hash AND t2.id < t1.id AND t1.audio_hash IS NOT NULL))",
                [],
            ).map_err(|e| e.to_string())?;
        }
        Ok(count)
    }
}

fn row_to_track(row: &rusqlite::Row) -> Track {
    Track {
        id: row.get(0).ok(),
        title: row.get(1).unwrap_or_default(),
        album_id: row.get(2).ok(),
        album_title: row.get(3).ok(),
        artist_id: row.get(4).ok(),
        artist_name: row.get(5).ok(),
        album_artist: row.get(6).ok(),
        disc_number: row.get(7).unwrap_or(1),
        disc_subtitle: row.get(8).ok(),
        track_number: row.get(9).unwrap_or(0),
        duration_ms: row.get(10).unwrap_or(0),
        file_path: row.get(11).ok(),
        format: row.get(12).ok(),
        sample_rate: row.get(13).ok(),
        bit_depth: row.get(14).ok(),
        channels: row.get(15).unwrap_or(2),
        file_mtime: row.get(16).ok(),
        file_size: row.get(17).ok(),
        audio_hash: row.get(18).ok(),
        source: row.get(19).unwrap_or_else(|_| "local".into()),
        source_id: row.get(20).ok(),
        isrc: row.get(21).ok(),
        genre: row.get(22).ok(),
        composer: row.get(23).ok(),
        year: row.get(24).ok(),
        bpm: row.get(25).ok(),
        label: row.get(26).ok(),
        musicbrainz_recording_id: row.get(27).ok(),
        cover_path: row.get(28).ok(),
        genres: row.get(29).ok().flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn crud_track() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("DSOTM", aid, Some(1973))
            .unwrap()
            .id
            .unwrap();

        let mut track = Track::new("Time".into());
        track.artist_id = Some(aid);
        track.album_id = Some(alid);
        track.file_path = Some("/music/pink_floyd/dsotm/time.flac".into());
        track.duration_ms = 413000;
        track.sample_rate = Some(44100);
        track.bit_depth = Some(16);

        let id = repo.create(&track).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "Time");
        assert_eq!(fetched.artist_name.as_deref(), Some("Pink Floyd"));
        assert_eq!(fetched.album_title.as_deref(), Some("DSOTM"));

        let by_path = repo
            .get_by_path("/music/pink_floyd/dsotm/time.flac")
            .unwrap();
        assert!(by_path.is_some());

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn get_all_paths() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Song 1".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("Song 2".into());
        t2.file_path = Some("/b.flac".into());

        repo.create(&t1).unwrap();
        repo.create(&t2).unwrap();

        let paths = repo.get_all_paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("/a.flac"));
    }

    #[test]
    fn get_multiple_preserves_order() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Alpha".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = Track::new("Beta".into());
        t2.file_path = Some("/2.flac".into());
        let mut t3 = Track::new("Gamma".into());
        t3.file_path = Some("/3.flac".into());

        let id1 = repo.create(&t1).unwrap();
        let id2 = repo.create(&t2).unwrap();
        let id3 = repo.create(&t3).unwrap();

        let result = repo.get_multiple(&[id3, id1, id2]).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].title, "Gamma");
        assert_eq!(result[1].title, "Alpha");
        assert_eq!(result[2].title, "Beta");
    }

    #[test]
    fn search_tracks() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Comfortably Numb".into());
        t.file_path = Some("/numb.flac".into());
        repo.create(&t).unwrap();

        let results = repo.search("comfort", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn track_count() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        let mut t = Track::new("A".into());
        t.file_path = Some("/a.flac".into());
        repo.create(&t).unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn track_list() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        for i in 0..5 {
            let mut t = Track::new(format!("Track {i}"));
            t.file_path = Some(format!("/{i}.flac"));
            repo.create(&t).unwrap();
        }

        let all = repo.list(100, 0).unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn track_list_pagination() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        for i in 0..10 {
            let mut t = Track::new(format!("Track {i:02}"));
            t.file_path = Some(format!("/{i}.flac"));
            repo.create(&t).unwrap();
        }

        let page1 = repo.list(3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = repo.list(3, 3).unwrap();
        assert_eq!(page2.len(), 3);
    }

    #[test]
    fn track_list_by_album() {
        let db = test_db();
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let alid = album_repo
            .create(&crate::db::models::Album::new("Album".into()))
            .unwrap();

        let mut t1 = Track::new("Track 1".into());
        t1.album_id = Some(alid);
        t1.disc_number = 1;
        t1.track_number = 2;
        t1.file_path = Some("/1-2.flac".into());
        repo.create(&t1).unwrap();

        let mut t2 = Track::new("Track 2".into());
        t2.album_id = Some(alid);
        t2.disc_number = 1;
        t2.track_number = 1;
        t2.file_path = Some("/1-1.flac".into());
        repo.create(&t2).unwrap();

        let tracks = repo.list_by_album(alid).unwrap();
        assert_eq!(tracks.len(), 2);
        // Should be sorted by disc, then track number
        assert_eq!(tracks[0].track_number, 1);
        assert_eq!(tracks[1].track_number, 2);
    }

    #[test]
    fn track_list_by_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();

        let mut t1 = Track::new("Song A".into());
        t1.artist_id = Some(aid);
        t1.file_path = Some("/a.flac".into());
        repo.create(&t1).unwrap();

        let mut t2 = Track::new("Song B".into());
        t2.artist_id = Some(aid);
        t2.file_path = Some("/b.flac".into());
        repo.create(&t2).unwrap();

        let tracks = repo.list_by_artist(aid).unwrap();
        assert_eq!(tracks.len(), 2);
    }

    #[test]
    fn track_update() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Original".into());
        t.file_path = Some("/orig.flac".into());
        let id = repo.create(&t).unwrap();

        let mut fetched = repo.get(id).unwrap().unwrap();
        fetched.title = "Updated".into();
        fetched.genre = Some("Jazz".into());
        fetched.composer = Some("Miles Davis".into());
        fetched.year = Some(1959);
        fetched.bpm = Some(120.5);
        repo.update(&fetched).unwrap();

        let updated = repo.get(id).unwrap().unwrap();
        assert_eq!(updated.title, "Updated");
        assert_eq!(updated.genre.as_deref(), Some("Jazz"));
        assert_eq!(updated.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(updated.year, Some(1959));
    }

    #[test]
    fn track_delete_by_path() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Test".into());
        t.file_path = Some("/test.flac".into());
        let id = repo.create(&t).unwrap();

        repo.delete_by_path("/test.flac").unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn track_get_all_local_file_info() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Test".into());
        t.file_path = Some("/music/test.flac".into());
        t.file_mtime = Some(1234567.89);
        t.file_size = Some(5_000_000);
        repo.create(&t).unwrap();

        let info = repo.get_all_local_file_info().unwrap();
        assert_eq!(info.len(), 1);
        let (tid, mtime, size) = info.get("/music/test.flac").unwrap();
        assert!(*tid > 0);
        assert_eq!(*mtime, Some(1234567.89));
        assert_eq!(*size, Some(5_000_000));
    }

    #[test]
    fn track_update_mtime_and_size() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Test".into());
        t.file_path = Some("/test.flac".into());
        let id = repo.create(&t).unwrap();

        repo.update_mtime_and_size("/test.flac", 999.99, 1_000_000)
            .unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.file_mtime, Some(999.99));
        assert_eq!(fetched.file_size, Some(1_000_000));
    }

    #[test]
    fn track_update_audio_hash() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Test".into());
        t.file_path = Some("/test.flac".into());
        let id = repo.create(&t).unwrap();

        repo.update_audio_hash("/test.flac", "abc123hash").unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.audio_hash.as_deref(), Some("abc123hash"));
    }

    #[test]
    fn track_deduplicate() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Song A".into());
        t1.file_path = Some("/a.flac".into());
        t1.audio_hash = Some("hash123".into());
        repo.create(&t1).unwrap();

        let mut t2 = Track::new("Song A Copy".into());
        t2.file_path = Some("/a_copy.flac".into());
        t2.audio_hash = Some("hash123".into());
        repo.create(&t2).unwrap();

        let mut t3 = Track::new("Song B".into());
        t3.file_path = Some("/b.flac".into());
        t3.audio_hash = Some("different".into());
        repo.create(&t3).unwrap();

        let removed = repo.deduplicate().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn track_random_ids() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        for i in 0..20 {
            let mut t = Track::new(format!("Track {i}"));
            t.file_path = Some(format!("/{i}.flac"));
            repo.create(&t).unwrap();
        }

        let ids = repo.random_ids(5).unwrap();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn track_get_multiple_empty() {
        let db = test_db();
        let repo = TrackRepo::new(db);
        let result = repo.get_multiple(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn track_get_nonexistent() {
        let db = test_db();
        let repo = TrackRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn track_get_by_path_nonexistent() {
        let db = test_db();
        let repo = TrackRepo::new(db);
        assert!(repo.get_by_path("/nonexistent.flac").unwrap().is_none());
    }

    #[test]
    fn track_with_all_metadata() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("Kind of Blue", aid, Some(1959))
            .unwrap()
            .id
            .unwrap();

        let mut t = Track::new("So What".into());
        t.artist_id = Some(aid);
        t.album_id = Some(alid);
        t.album_artist = Some("Miles Davis".into());
        t.disc_number = 1;
        t.track_number = 1;
        t.duration_ms = 562_000;
        t.file_path = Some("/music/miles/kob/so_what.flac".into());
        t.format = Some("flac".into());
        t.sample_rate = Some(96000);
        t.bit_depth = Some(24);
        t.channels = 2;
        t.genre = Some("Jazz".into());
        t.genres = Some(r#"["Jazz","Modal Jazz"]"#.into());
        t.composer = Some("Miles Davis".into());
        t.year = Some(1959);
        t.label = Some("Columbia".into());
        t.isrc = Some("US1234567890".into());
        t.source = "local".into();
        let id = repo.create(&t).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "So What");
        assert_eq!(fetched.artist_name.as_deref(), Some("Miles Davis"));
        assert_eq!(fetched.album_title.as_deref(), Some("Kind of Blue"));
        assert_eq!(fetched.album_artist.as_deref(), Some("Miles Davis"));
        assert_eq!(fetched.duration_ms, 562_000);
        assert_eq!(fetched.sample_rate, Some(96000));
        assert_eq!(fetched.bit_depth, Some(24));
        assert_eq!(fetched.genre.as_deref(), Some("Jazz"));
        assert_eq!(fetched.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(fetched.label.as_deref(), Some("Columbia"));
    }
}
