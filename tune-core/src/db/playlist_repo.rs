use rusqlite::{params, OptionalExtension};
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
        let conn = self.db.connection().lock().unwrap();
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
        let conn = self.db.connection().lock().unwrap();
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
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM playlists WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn update(&self, id: i64, name: Option<&str>, description: Option<&str>) -> Result<(), String> {
        if let Some(n) = name {
            self.db.execute("UPDATE playlists SET name = ? WHERE id = ?", &[&n as &dyn rusqlite::types::ToSql, &id])?;
        }
        if let Some(d) = description {
            self.db.execute("UPDATE playlists SET description = ? WHERE id = ?", &[&d as &dyn rusqlite::types::ToSql, &id])?;
        }
        Ok(())
    }

    pub fn add_tracks(&self, playlist_id: i64, track_ids: &[i64], position: Option<i64>) -> Result<Vec<i64>, String> {
        let conn = self.db.connection().lock().unwrap();
        let max_pos: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = ?",
                params![playlist_id],
                |row| row.get(0),
            )
            .unwrap_or(-1);

        let start_pos = position.unwrap_or(max_pos + 1);
        let mut inserted = Vec::new();
        for (i, tid) in track_ids.iter().enumerate() {
            let pos = start_pos + i as i64;
            conn.execute(
                "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?, ?, ?)",
                params![playlist_id, tid, pos],
            ).map_err(|e| e.to_string())?;
            inserted.push(*tid);
        }
        Ok(inserted)
    }

    pub fn remove_tracks_at_positions(&self, playlist_id: i64, positions: &[i64]) -> Result<usize, String> {
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
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map(params![playlist_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    pub fn reorder_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        conn.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?", params![playlist_id])
            .map_err(|e| e.to_string())?;
        for (i, tid) in track_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?, ?, ?)",
                params![playlist_id, tid, i as i64],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
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
}
