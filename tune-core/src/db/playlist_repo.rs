use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub track_count: i64,
}

pub struct PlaylistRepo {
    db: SqliteDb,
}

impl PlaylistRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn create(&self, name: &str, description: Option<&str>) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO playlists (name, description) VALUES (?, ?)",
            &[&name as &dyn rusqlite::types::ToSql, &description],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Playlist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.id = ?")
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| {
            Ok(Playlist {
                id: row.get(0).ok(),
                name: row.get(1).unwrap_or_default(),
                description: row.get(2).ok(),
                track_count: row.get(3).unwrap_or(0),
            })
        })
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Playlist>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p ORDER BY p.name COLLATE NOCASE LIMIT ? OFFSET ?")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![limit, offset], |row| {
                Ok(Playlist {
                    id: row.get(0).ok(),
                    name: row.get(1).unwrap_or_default(),
                    description: row.get(2).ok(),
                    track_count: row.get(3).unwrap_or(0),
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db
            .execute("DELETE FROM playlists WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn update(
        &self,
        id: i64,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        if let Some(n) = name {
            self.db.execute(
                "UPDATE playlists SET name = ? WHERE id = ?",
                &[&n as &dyn rusqlite::types::ToSql, &id],
            )?;
        }
        if let Some(d) = description {
            self.db.execute(
                "UPDATE playlists SET description = ? WHERE id = ?",
                &[&d as &dyn rusqlite::types::ToSql, &id],
            )?;
        }
        Ok(())
    }

    pub fn add_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<Vec<i64>, String> {
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let max_pos: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = ?",
                params![playlist_id],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let start_pos = position.unwrap_or(max_pos + 1);
        let mut inserted = Vec::new();
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?, ?, ?)")
                .map_err(|e| e.to_string())?;
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = start_pos + i as i64;
                stmt.execute(params![playlist_id, tid, pos])
                    .map_err(|e| e.to_string())?;
                inserted.push(*tid);
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(inserted)
    }

    pub fn remove_tracks_at_positions(
        &self,
        playlist_id: i64,
        positions: &[i64],
    ) -> Result<usize, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut removed = 0usize;
        for pos in positions {
            let n = conn
                .execute(
                    "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND position = ?2",
                    params![playlist_id, pos],
                )
                .map_err(|e| e.to_string())?;
            removed += n;
        }
        Ok(removed)
    }

    pub fn remove_track(&self, playlist_id: i64, position: i64) -> Result<(), String> {
        self.db.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ? AND position = ?",
            &[&playlist_id, &position],
        )?;
        Ok(())
    }

    pub fn get_track_ids(&self, playlist_id: i64) -> Result<Vec<i64>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map(params![playlist_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(ids)
    }

    pub fn reorder_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let mut conn = self.db.connection().lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?",
            params![playlist_id],
        )
        .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?, ?, ?)")
                .map_err(|e| e.to_string())?;
            for (i, tid) in track_ids.iter().enumerate() {
                stmt.execute(params![playlist_id, tid, i as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM playlists", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Track as TrackModel;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn crud_playlist() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let id = repo.create("My Playlist", Some("Test")).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "My Playlist");
        assert_eq!(pl.track_count, 0);

        repo.update(id, Some("Renamed"), None).unwrap();
        let pl2 = repo.get(id).unwrap().unwrap();
        assert_eq!(pl2.name, "Renamed");

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn playlist_tracks() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("Song A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("Song B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test PL", None).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();

        let ids = repo.get_track_ids(plid).unwrap();
        assert_eq!(ids, vec![tid1, tid2]);

        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 2);

        repo.reorder_tracks(plid, &[tid2, tid1]).unwrap();
        let reordered = repo.get_track_ids(plid).unwrap();
        assert_eq!(reordered, vec![tid2, tid1]);
    }

    #[test]
    fn playlist_count() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        repo.create("Playlist 1", None).unwrap();
        repo.create("Playlist 2", None).unwrap();
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn playlist_list() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        repo.create("Zebra", None).unwrap();
        repo.create("Alpha", None).unwrap();
        repo.create("Middle", None).unwrap();

        let all = repo.list(100, 0).unwrap();
        assert_eq!(all.len(), 3);
        // Sorted by name NOCASE
        assert_eq!(all[0].name, "Alpha");
        assert_eq!(all[2].name, "Zebra");
    }

    #[test]
    fn playlist_list_pagination() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        for i in 0..10 {
            repo.create(&format!("PL {i:02}"), None).unwrap();
        }

        let page1 = repo.list(3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = repo.list(3, 3).unwrap();
        assert_eq!(page2.len(), 3);
        assert_ne!(page1[0].name, page2[0].name);
    }

    #[test]
    fn playlist_update_description() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let id = repo.create("Test", Some("Initial")).unwrap();
        repo.update(id, None, Some("Updated desc")).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "Test"); // Name unchanged
        assert_eq!(pl.description.as_deref(), Some("Updated desc"));
    }

    #[test]
    fn playlist_add_tracks_at_position() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let mut t3 = TrackModel::new("C".into());
        t3.file_path = Some("/c.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        let plid = repo.create("Test", None).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();
        // Insert tid3 at position 1
        repo.add_tracks(plid, &[tid3], Some(1)).unwrap();

        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 3);
    }

    #[test]
    fn playlist_remove_track() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test", None).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();
        repo.remove_track(plid, 0).unwrap();

        let ids = repo.get_track_ids(plid).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], tid2);
    }

    #[test]
    fn playlist_remove_tracks_at_positions() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/2.flac".into());
        let mut t3 = TrackModel::new("C".into());
        t3.file_path = Some("/3.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        let plid = repo.create("Test", None).unwrap();
        repo.add_tracks(plid, &[tid1, tid2, tid3], None).unwrap();
        let removed = repo.remove_tracks_at_positions(plid, &[0, 2]).unwrap();
        assert_eq!(removed, 2);

        let remaining = repo.get_track_ids(plid).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0], tid2);
    }

    #[test]
    fn playlist_empty_name() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        // Empty name should still work
        let id = repo.create("", None).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "");
    }

    #[test]
    fn playlist_unicode_name() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let id = repo
            .create("Ma playlist preferee", Some("Musique francaise"))
            .unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "Ma playlist preferee");
    }

    #[test]
    fn playlist_delete_cascade() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t = TrackModel::new("Track".into());
        t.file_path = Some("/t.flac".into());
        let tid = track_repo.create(&t).unwrap();

        let plid = repo.create("Test", None).unwrap();
        repo.add_tracks(plid, &[tid], None).unwrap();
        repo.delete(plid).unwrap();

        // Playlist tracks should also be deleted (CASCADE)
        assert!(repo.get(plid).unwrap().is_none());
    }

    #[test]
    fn get_nonexistent_playlist() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }
}
