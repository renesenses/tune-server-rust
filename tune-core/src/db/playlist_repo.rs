use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for playlist_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO playlists (name, description, profile_id) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.id = {}",
            d.placeholder(1)
        )
    }

    pub fn list<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.profile_id = {} ORDER BY LOWER(p.name) LIMIT {} OFFSET {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM playlists WHERE id = {}", d.placeholder(1))
    }

    pub fn update_field<D: SqlDialect>(d: &D, field: &str) -> String {
        format!(
            "UPDATE playlists SET {field} = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn max_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = {}",
            d.placeholder(1)
        )
    }

    pub fn insert_playlist_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_track_at_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlist_tracks WHERE playlist_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_track_ids<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = {} ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn delete_all_tracks<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlist_tracks WHERE playlist_id = {}",
            d.placeholder(1)
        )
    }

    pub fn count<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM playlists WHERE profile_id = {}",
            d.placeholder(1)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub track_count: i64,
}

pub struct PlaylistRepo {
    db: Arc<dyn DbBackend>,
}

impl PlaylistRepo {
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

    pub fn create(
        &self,
        name: &str,
        description: Option<&str>,
        profile_id: i64,
    ) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 3] = [&name, &description, &profile_id];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Playlist>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_playlist))
    }

    pub fn list(&self, profile_id: i64, limit: i64, offset: i64) -> Result<Vec<Playlist>, String> {
        let sql = self.dialect_sql(sql::list, sql::list);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_playlist).collect())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update(
        &self,
        id: i64,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        if let Some(n) = name {
            let sql = self.dialect_sql(
                |d| sql::update_field(d, "name"),
                |d| sql::update_field(d, "name"),
            );
            let params: [&dyn ToSqlValue; 2] = [&n, &id];
            self.db.execute(&sql, &params)?;
        }
        if let Some(d) = description {
            let sql = self.dialect_sql(
                |dlc| sql::update_field(dlc, "description"),
                |dlc| sql::update_field(dlc, "description"),
            );
            let params: [&dyn ToSqlValue; 2] = [&d, &id];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    pub fn add_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<Vec<i64>, String> {
        let max_pos_sql = self.dialect_sql(sql::max_position, sql::max_position);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        let mut inserted = Vec::with_capacity(track_ids.len());
        let inserted_ref = &mut inserted;
        self.db.write_tx(&mut |tx| {
            let max_pos_params: [&dyn ToSqlValue; 1] = [&playlist_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &max_pos_params)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start_pos = position.unwrap_or(max_pos + 1);
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = start_pos + i as i64;
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
                inserted_ref.push(*tid);
            }
            Ok(())
        })?;
        Ok(inserted)
    }

    /// Like `add_tracks` but skips tracks already in the playlist and repeats
    /// within the batch, so a playlist never holds the same track twice. This
    /// is the path for user "add to playlist" actions (duplicates also made
    /// "remove" look broken — removing one position left the other copy behind,
    /// Elie). Raw `add_tracks` is kept for flows that intentionally preserve
    /// duplicates (e.g. merge-without-dedup).
    pub fn add_tracks_deduped(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<Vec<i64>, String> {
        let existing: std::collections::HashSet<i64> =
            self.get_track_ids(playlist_id)?.into_iter().collect();
        let mut batch_seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let to_add: Vec<i64> = track_ids
            .iter()
            .copied()
            .filter(|tid| !existing.contains(tid) && batch_seen.insert(*tid))
            .collect();
        if to_add.is_empty() {
            return Ok(Vec::new());
        }
        self.add_tracks(playlist_id, &to_add, position)
    }

    pub fn remove_tracks_at_positions(
        &self,
        playlist_id: i64,
        positions: &[i64],
    ) -> Result<usize, String> {
        let delete_sql =
            self.dialect_sql(sql::delete_track_at_position, sql::delete_track_at_position);
        let mut removed = 0usize;
        let removed_ref = &mut removed;
        self.db.write_tx(&mut |tx| {
            for pos in positions {
                let p: [&dyn ToSqlValue; 2] = [&playlist_id, pos];
                *removed_ref += tx.execute(&delete_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(removed)
    }

    pub fn remove_track(&self, playlist_id: i64, position: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_track_at_position, sql::delete_track_at_position);
        let params: [&dyn ToSqlValue; 2] = [&playlist_id, &position];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_track_ids(&self, playlist_id: i64) -> Result<Vec<i64>, String> {
        let sql = self.dialect_sql(sql::get_track_ids, sql::get_track_ids);
        let params: [&dyn ToSqlValue; 1] = [&playlist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn reorder_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let delete_sql = self.dialect_sql(sql::delete_all_tracks, sql::delete_all_tracks);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&playlist_id];
            tx.execute(&delete_sql, &p)?;
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn count(&self, profile_id: i64) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::count, sql::count);
        let params: [&dyn ToSqlValue; 1] = [&profile_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }
}

fn row_to_playlist(cols: &Vec<SqlValue>) -> Playlist {
    Playlist {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        description: cols.get(2).and_then(|v| v.as_string()),
        track_count: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
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

        let id = repo.create("My Playlist", Some("Test"), 1).unwrap();
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

        let plid = repo.create("Test PL", None, 1).unwrap();
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

        assert_eq!(repo.count(1).unwrap(), 0);
        repo.create("Playlist 1", None, 1).unwrap();
        repo.create("Playlist 2", None, 1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 2);
    }

    #[test]
    fn playlist_list() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        repo.create("Zebra", None, 1).unwrap();
        repo.create("Alpha", None, 1).unwrap();
        repo.create("Middle", None, 1).unwrap();

        let all = repo.list(1, 100, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "Alpha");
        assert_eq!(all[2].name, "Zebra");
    }

    #[test]
    fn playlist_scoped_by_profile() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        repo.create("P1 only", None, 1).unwrap();
        repo.create("P2 only", None, 2).unwrap();
        repo.create("P2 second", None, 2).unwrap();

        // list + count are scoped to the requesting profile.
        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count(2).unwrap(), 2);
        let p1 = repo.list(1, 100, 0).unwrap();
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0].name, "P1 only");
        let p2 = repo.list(2, 100, 0).unwrap();
        assert_eq!(p2.len(), 2);
    }

    #[test]
    fn playlist_list_pagination() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        for i in 0..10 {
            repo.create(&format!("PL {i:02}"), None, 1).unwrap();
        }

        let page1 = repo.list(1, 3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = repo.list(1, 3, 3).unwrap();
        assert_eq!(page2.len(), 3);
        assert_ne!(page1[0].name, page2[0].name);
    }

    #[test]
    fn playlist_update_description() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let id = repo.create("Test", Some("Initial"), 1).unwrap();
        repo.update(id, None, Some("Updated desc")).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "Test");
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

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();
        repo.add_tracks(plid, &[tid3], Some(1)).unwrap();

        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 3);
    }

    #[test]
    fn playlist_add_tracks_skips_duplicates() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        // Duplicate within a single batch → inserted once.
        let added = repo
            .add_tracks_deduped(plid, &[tid1, tid1, tid2], None)
            .unwrap();
        assert_eq!(added, vec![tid1, tid2]);
        // Re-adding an existing track → skipped; only the new one lands.
        let added2 = repo.add_tracks_deduped(plid, &[tid1, tid2], None).unwrap();
        assert!(added2.is_empty());
        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 2);
        assert_eq!(repo.get_track_ids(plid).unwrap(), vec![tid1, tid2]);
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

        let plid = repo.create("Test", None, 1).unwrap();
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

        let plid = repo.create("Test", None, 1).unwrap();
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
        let id = repo.create("", None, 1).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "");
    }

    #[test]
    fn playlist_unicode_name() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let id = repo
            .create("Ma playlist preferee", Some("Musique francaise"), 1)
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

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid], None).unwrap();
        repo.delete(plid).unwrap();

        assert!(repo.get(plid).unwrap().is_none());
    }

    #[test]
    fn get_nonexistent_playlist() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create(&s).contains("VALUES (?, ?, ?)"));
        assert!(sql::create(&p).contains("VALUES ($1, $2, $3)"));
        assert!(sql::create(&s).contains("profile_id"));
        assert!(!sql::list(&p).contains("COLLATE"));
        assert!(sql::list(&p).contains("LOWER(p.name)"));
        assert!(sql::list(&p).contains("profile_id ="));
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = PlaylistRepo::with_backend(backend);
        let id = repo.create("X", None, 1).unwrap();
        assert!(repo.get(id).unwrap().is_some());
    }
}
