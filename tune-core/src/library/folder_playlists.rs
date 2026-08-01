//! Folder → local playlist discovery, run after a scan (Frédéric Fongarnand).
//!
//! A directory whose files span SEVERAL library albums is treated as a
//! hand-made compilation ("dossier-playlist") and mirrored into a local
//! playlist named after the directory. Regular album folders (all tracks on
//! one album, disc subfolders included) never qualify, so a clean library
//! grows zero playlists. The sync is idempotent: the playlist is keyed by its
//! description (`Dossier : <path>`) and its contents are replaced on every
//! scan to mirror the directory.
//!
//! Gated by the `scan_folder_playlists` setting (default OFF).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::settings_repo::SettingsRepo;
use tracing::{info, warn};

/// Default profile a scan-discovered playlist is attached to (Default profile).
const DEFAULT_PROFILE_ID: i64 = 1;

const SETTING_KEY: &str = "scan_folder_playlists";
const DESC_PREFIX: &str = "Dossier : ";
/// A folder needs at least this many direct tracks to become a playlist.
const MIN_TRACKS: usize = 3;
/// … and its tracks must span at least this many distinct albums.
const MIN_DISTINCT_ALBUMS: usize = 2;

pub fn folder_playlists_enabled(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// One library track as read from the DB: (id, file_path, album_id).
type TrackRow = (i64, String, Option<i64>);

/// Pure decision logic, unit-tested: group tracks by parent directory and
/// keep the directories that look like hand-made compilations. Track ids are
/// ordered by file name so the playlist follows the on-disk ordering.
fn candidate_dirs(rows: &[TrackRow]) -> Vec<(String, String, Vec<i64>)> {
    let mut by_dir: BTreeMap<String, Vec<(&str, i64, Option<i64>)>> = BTreeMap::new();
    for (id, path, album_id) in rows {
        let p = std::path::Path::new(path);
        let (Some(parent), Some(file)) = (p.parent(), p.file_name()) else {
            continue;
        };
        let Some(file) = file.to_str() else { continue };
        by_dir
            .entry(parent.to_string_lossy().into_owned())
            .or_default()
            .push((file, *id, *album_id));
    }

    let mut out = Vec::new();
    for (dir, mut tracks) in by_dir {
        if tracks.len() < MIN_TRACKS {
            continue;
        }
        let mut albums: Vec<Option<i64>> = tracks.iter().map(|(_, _, a)| *a).collect();
        albums.sort_unstable();
        albums.dedup();
        if albums.len() < MIN_DISTINCT_ALBUMS {
            continue;
        }
        let Some(name) = std::path::Path::new(&dir)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        tracks.sort_by(|a, b| a.0.cmp(b.0));
        let ids = tracks.iter().map(|(_, id, _)| *id).collect();
        out.push((dir, name, ids));
    }
    out
}

/// Discover folder playlists and mirror them into local playlists.
/// Called at the end of manual and automatic scans when the setting is on.
pub fn sync_folder_playlists(db: &Arc<dyn DbBackend>) {
    let rows = match db.query_many(
        "SELECT id, file_path, album_id FROM tracks \
         WHERE source = 'local' AND file_path IS NOT NULL",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "folder_playlists_track_query_failed");
            return;
        }
    };
    let tracks: Vec<TrackRow> = rows
        .iter()
        .filter_map(|cols| {
            Some((
                cols.first()?.as_i64()?,
                cols.get(1)?.as_str()?.to_owned(),
                cols.get(2).and_then(|v| v.as_i64()),
            ))
        })
        .collect();

    let candidates = candidate_dirs(&tracks);
    if candidates.is_empty() {
        return;
    }

    let repo = PlaylistRepo::with_backend(db.clone());
    let existing = match repo.list(DEFAULT_PROFILE_ID, 10_000, 0) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "folder_playlists_list_failed");
            return;
        }
    };

    let mut created = 0usize;
    let mut updated = 0usize;
    for (dir, name, ids) in candidates {
        let desc = format!("{DESC_PREFIX}{dir}");
        let found = existing
            .iter()
            .find(|p| p.description.as_deref() == Some(desc.as_str()));
        let playlist_id = match found {
            Some(p) => match p.id {
                Some(id) => id,
                None => continue,
            },
            None => match repo.create(&name, Some(&desc), DEFAULT_PROFILE_ID) {
                Ok(id) => {
                    created += 1;
                    id
                }
                Err(e) => {
                    warn!(dir = %dir, error = %e, "folder_playlist_create_failed");
                    continue;
                }
            },
        };
        if repo.get_track_ids(playlist_id).ok().as_deref() == Some(ids.as_slice()) {
            continue;
        }
        match repo.set_tracks(playlist_id, &ids) {
            Ok(()) => {
                if found.is_some() {
                    updated += 1;
                }
            }
            Err(e) => warn!(dir = %dir, error = %e, "folder_playlist_sync_failed"),
        }
    }
    if created > 0 || updated > 0 {
        info!(created, updated, "folder_playlists_synced");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, path: &str, album: Option<i64>) -> TrackRow {
        (id, path.to_owned(), album)
    }

    #[test]
    fn album_folder_is_not_a_playlist() {
        // One album spread over a folder (and a disc subfolder) → no candidate.
        let rows = vec![
            row(1, "/music/Artist/Album/01.flac", Some(7)),
            row(2, "/music/Artist/Album/02.flac", Some(7)),
            row(3, "/music/Artist/Album/03.flac", Some(7)),
            row(4, "/music/Artist/Album/CD2/01.flac", Some(7)),
            row(5, "/music/Artist/Album/CD2/02.flac", Some(7)),
            row(6, "/music/Artist/Album/CD2/03.flac", Some(7)),
        ];
        assert!(candidate_dirs(&rows).is_empty());
    }

    #[test]
    fn compilation_folder_becomes_playlist_in_filename_order() {
        let rows = vec![
            row(10, "/music/Playlists/Soirée/03 - C.flac", Some(1)),
            row(11, "/music/Playlists/Soirée/01 - A.flac", Some(2)),
            row(12, "/music/Playlists/Soirée/02 - B.flac", None),
        ];
        let out = candidate_dirs(&rows);
        assert_eq!(out.len(), 1);
        let (dir, name, ids) = &out[0];
        assert_eq!(dir, "/music/Playlists/Soirée");
        assert_eq!(name, "Soirée");
        assert_eq!(ids, &vec![11, 12, 10]);
    }

    #[test]
    fn too_few_tracks_or_albums_is_skipped() {
        // 2 tracks over 2 albums: below MIN_TRACKS.
        let rows = vec![
            row(1, "/m/d/a.flac", Some(1)),
            row(2, "/m/d/b.flac", Some(2)),
        ];
        assert!(candidate_dirs(&rows).is_empty());
    }
}
