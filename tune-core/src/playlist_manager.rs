use std::path::Path;

use tracing::info;

use crate::db::playlist_repo::PlaylistRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

pub struct PlaylistManager {
    db: SqliteDb,
}

impl PlaylistManager {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn export_m3u(&self, playlist_id: i64) -> Result<String, String> {
        let repo = PlaylistRepo::new(self.db.clone());
        let playlist = repo.get(playlist_id)?.ok_or("playlist not found")?;

        let track_ids = repo.get_track_ids(playlist_id)?;
        let track_repo = TrackRepo::new(self.db.clone());

        let mut lines = Vec::new();
        lines.push("#EXTM3U".to_string());
        lines.push(format!("#PLAYLIST:{}", playlist.name));

        for tid in &track_ids {
            if let Ok(Some(track)) = track_repo.get(*tid) {
                let duration_s = track.duration_ms / 1000;
                let artist = track.artist_name.as_deref().unwrap_or("Unknown");
                lines.push(format!(
                    "#EXTINF:{},{} - {}",
                    duration_s, artist, track.title
                ));
                if let Some(ref path) = track.file_path {
                    lines.push(path.clone());
                }
            }
        }

        Ok(lines.join("\n"))
    }

    pub fn import_m3u(&self, content: &str, playlist_name: &str) -> Result<ImportResult, String> {
        let repo = PlaylistRepo::new(self.db.clone());
        let track_repo = TrackRepo::new(self.db.clone());

        // tune-core has no request context; server-side playlist creation
        // belongs to the built-in Default profile (1).
        let playlist_id = repo.create(playlist_name, None, 1)?;
        let mut matched = 0usize;
        let mut skipped = 0usize;
        let mut track_ids = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            match track_repo.find_by_path(line) {
                Ok(Some(track)) => {
                    if let Some(id) = track.id {
                        track_ids.push(id);
                        matched += 1;
                    }
                }
                _ => {
                    let filename = Path::new(line)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(line);

                    match track_repo.search_by_title(filename, 1) {
                        Ok(tracks) if !tracks.is_empty() => {
                            if let Some(id) = tracks[0].id {
                                track_ids.push(id);
                                matched += 1;
                            }
                        }
                        _ => {
                            skipped += 1;
                        }
                    }
                }
            }
        }

        if !track_ids.is_empty() {
            repo.add_tracks(playlist_id, &track_ids, None)?;
        }

        info!(
            playlist_id,
            name = playlist_name,
            matched,
            skipped,
            "m3u_import_complete"
        );

        Ok(ImportResult {
            playlist_id,
            matched,
            skipped,
        })
    }

    pub fn duplicate(&self, playlist_id: i64, new_name: &str) -> Result<i64, String> {
        let repo = PlaylistRepo::new(self.db.clone());
        let original = repo.get(playlist_id)?.ok_or("playlist not found")?;

        let new_id = repo.create(new_name, original.description.as_deref(), 1)?;
        let track_ids = repo.get_track_ids(playlist_id)?;
        if !track_ids.is_empty() {
            repo.add_tracks(new_id, &track_ids, None)?;
        }

        info!(original_id = playlist_id, new_id, "playlist_duplicated");
        Ok(new_id)
    }

    pub fn merge(
        &self,
        source_ids: &[i64],
        target_name: &str,
        deduplicate: bool,
    ) -> Result<i64, String> {
        let repo = PlaylistRepo::new(self.db.clone());
        let target_id = repo.create(target_name, None, 1)?;

        let mut all_tracks = Vec::new();
        for &src_id in source_ids {
            let tracks = repo.get_track_ids(src_id)?;
            all_tracks.extend(tracks);
        }

        if deduplicate {
            let mut seen = std::collections::HashSet::new();
            all_tracks.retain(|id| seen.insert(*id));
        }

        if !all_tracks.is_empty() {
            repo.add_tracks(target_id, &all_tracks, None)?;
        }

        info!(
            sources = ?source_ids,
            target_id,
            tracks = all_tracks.len(),
            "playlists_merged"
        );
        Ok(target_id)
    }
}

#[derive(Debug)]
pub struct ImportResult {
    pub playlist_id: i64,
    pub matched: usize,
    pub skipped: usize,
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

    fn insert_track(db: &SqliteDb, title: &str, path: &str) -> i64 {
        let repo = TrackRepo::new(db.clone());
        let mut t = TrackModel::new(title.into());
        t.file_path = Some(path.into());
        t.duration_ms = 180_000;
        t.artist_name = Some("Artist".into());
        repo.create(&t).unwrap()
    }

    #[test]
    fn export_m3u() {
        let db = test_db();
        let t1 = insert_track(&db, "Song One", "/music/song1.flac");
        let t2 = insert_track(&db, "Song Two", "/music/song2.flac");

        let repo = PlaylistRepo::new(db.clone());
        let plid = repo.create("Test Export", None, 1).unwrap();
        repo.add_tracks(plid, &[t1, t2], None).unwrap();

        let mgr = PlaylistManager::new(db);
        let m3u = mgr.export_m3u(plid).unwrap();

        assert!(m3u.starts_with("#EXTM3U"));
        assert!(m3u.contains("#PLAYLIST:Test Export"));
        assert!(m3u.contains("/music/song1.flac"));
        assert!(m3u.contains("/music/song2.flac"));
        assert!(m3u.contains("#EXTINF:180,Unknown - Song One"));
    }

    #[test]
    fn duplicate_playlist() {
        let db = test_db();
        let t1 = insert_track(&db, "Track A", "/a.flac");
        let t2 = insert_track(&db, "Track B", "/b.flac");

        let repo = PlaylistRepo::new(db.clone());
        let plid = repo.create("Original", Some("Desc"), 1).unwrap();
        repo.add_tracks(plid, &[t1, t2], None).unwrap();

        let mgr = PlaylistManager::new(db.clone());
        let new_id = mgr.duplicate(plid, "Copy").unwrap();

        let copy = repo.get(new_id).unwrap().unwrap();
        assert_eq!(copy.name, "Copy");
        assert_eq!(copy.track_count, 2);

        let tracks = repo.get_track_ids(new_id).unwrap();
        assert_eq!(tracks, vec![t1, t2]);
    }

    #[test]
    fn merge_playlists() {
        let db = test_db();
        let t1 = insert_track(&db, "A", "/a.flac");
        let t2 = insert_track(&db, "B", "/b.flac");
        let t3 = insert_track(&db, "C", "/c.flac");

        let repo = PlaylistRepo::new(db.clone());
        let pl1 = repo.create("PL1", None, 1).unwrap();
        repo.add_tracks(pl1, &[t1, t2], None).unwrap();
        let pl2 = repo.create("PL2", None, 1).unwrap();
        repo.add_tracks(pl2, &[t2, t3], None).unwrap();

        let mgr = PlaylistManager::new(db.clone());

        let merged = mgr.merge(&[pl1, pl2], "Merged", true).unwrap();
        let tracks = repo.get_track_ids(merged).unwrap();
        assert_eq!(tracks.len(), 3); // deduplicated
        assert_eq!(tracks, vec![t1, t2, t3]);
    }

    #[test]
    fn merge_without_dedup() {
        let db = test_db();
        let t1 = insert_track(&db, "A", "/a.flac");

        let repo = PlaylistRepo::new(db.clone());
        let pl1 = repo.create("PL1", None, 1).unwrap();
        repo.add_tracks(pl1, &[t1], None).unwrap();
        let pl2 = repo.create("PL2", None, 1).unwrap();
        repo.add_tracks(pl2, &[t1], None).unwrap();

        let mgr = PlaylistManager::new(db.clone());
        let merged = mgr.merge(&[pl1, pl2], "Merged", false).unwrap();
        let tracks = repo.get_track_ids(merged).unwrap();
        assert_eq!(tracks.len(), 2); // t1 appears twice
    }

    #[test]
    fn import_m3u_by_path() {
        let db = test_db();
        let _t1 = insert_track(&db, "Found", "/music/found.flac");

        let m3u = "#EXTM3U\n#EXTINF:180,Artist - Found\n/music/found.flac\n/music/missing.flac\n";
        let mgr = PlaylistManager::new(db.clone());
        let result = mgr.import_m3u(m3u, "Imported").unwrap();

        assert_eq!(result.matched, 1);
        assert_eq!(result.skipped, 1);

        let repo = PlaylistRepo::new(db);
        let pl = repo.get(result.playlist_id).unwrap().unwrap();
        assert_eq!(pl.name, "Imported");
        assert_eq!(pl.track_count, 1);
    }
}
