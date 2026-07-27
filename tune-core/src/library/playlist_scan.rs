//! Import playlist files (`.m3u` / `.m3u8` / `.pls`) found while scanning music
//! directories, turning each into a local Tune playlist (Bertrand: "les
//! playlists rencontrées au scan doivent être ajoutées en playlists locales").
//!
//! Reuses the existing parse + match engine (`library::importer`): each playlist
//! file's entries are resolved (relative paths against the playlist's own folder)
//! and matched against the library by file path first, then a title/artist/album
//! fuzzy fallback. Only playlists with at least one matched track are created,
//! and a playlist whose name already exists is skipped so a re-scan never
//! duplicates it.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::db::backend::DbBackend;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::track_repo::TrackRepo;
use crate::library::importer::{ImportedTrack, match_tracks, parse_m3u, parse_pls};

/// Default profile a scan-imported playlist is attached to (Default profile).
const DEFAULT_PROFILE_ID: i64 = 1;

#[derive(Debug, Default)]
pub struct PlaylistScanReport {
    pub playlists_created: usize,
    pub tracks_added: usize,
}

/// Canonical comparison key for a file path: forward slashes + NFC. Both the
/// library index and the resolved playlist entries go through this so paths
/// compare equal regardless of separator/normalisation origin.
fn path_key(p: &str) -> String {
    p.replace('\\', "/").nfc().collect()
}

/// Lexically resolve `..` / `.` without touching the filesystem (the referenced
/// file may have moved; we only want a comparable absolute-ish path).
fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve one playlist entry to a comparison key, or `None` for URLs / empties.
fn resolve_entry(entry: &str, playlist_dir: &Path) -> Option<String> {
    let e = entry.trim();
    if e.is_empty() || e.starts_with("http://") || e.starts_with("https://") {
        return None;
    }
    let p = Path::new(e);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        playlist_dir.join(p)
    };
    Some(path_key(&lexical_clean(&abs).to_string_lossy()))
}

/// Scan `dirs` for playlist files and create a local playlist for each one that
/// resolves to at least one library track. Idempotent by playlist name.
pub fn import_local_playlists(db: &Arc<dyn DbBackend>, dirs: &[String]) -> PlaylistScanReport {
    let mut report = PlaylistScanReport::default();
    let track_repo = TrackRepo::with_backend(db.clone());
    let playlist_repo = PlaylistRepo::with_backend(db.clone());

    // Build the match indexes once from the freshly-scanned library.
    let tracks = match track_repo.list_all_local() {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "playlist_scan_load_tracks_failed");
            return report;
        }
    };
    let mut path_index: HashMap<String, i64> = HashMap::with_capacity(tracks.len());
    let mut fuzzy_index: HashMap<(String, String, String), i64> = HashMap::new();
    for t in &tracks {
        let Some(id) = t.id else { continue };
        if let Some(fp) = &t.file_path {
            path_index.insert(path_key(fp), id);
        }
        let key = (
            t.title.to_lowercase(),
            t.artist_name.clone().unwrap_or_default().to_lowercase(),
            t.album_title.clone().unwrap_or_default().to_lowercase(),
        );
        fuzzy_index.entry(key).or_insert(id);
    }

    // Existing playlist names (default profile) — skip to stay idempotent.
    let existing: HashSet<String> = playlist_repo
        .list(DEFAULT_PROFILE_ID, 100_000, 0)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.name)
        .collect();

    for pf in find_playlist_files(dirs) {
        let Some(name) = pf
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if existing.contains(&name) {
            continue;
        }

        let raw = match std::fs::read(&pf) {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %pf.display(), error = %e, "playlist_file_read_failed");
                continue;
            }
        };
        let text = String::from_utf8_lossy(&raw);
        let is_pls = pf
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pls"));
        let mut imported: Vec<ImportedTrack> = if is_pls {
            parse_pls(&text, Some(&name))
        } else {
            parse_m3u(&text, Some(&name))
        };

        // Resolve each entry's file_path (relative → absolute key). Entries that
        // are URLs (radio streams) drop their file_path so only the title/artist
        // fuzzy fallback can match them (usually it won't — that's fine).
        let playlist_dir = pf.parent().unwrap_or_else(|| Path::new(""));
        for it in &mut imported {
            it.file_path = it
                .file_path
                .as_deref()
                .and_then(|fp| resolve_entry(fp, playlist_dir));
        }

        let results = match_tracks(&imported, &path_index, &fuzzy_index);
        // Preserve playlist order, drop unmatched, dedup repeats.
        let mut seen = HashSet::new();
        let track_ids: Vec<i64> = results
            .iter()
            .filter_map(|r| r.tune_track_id)
            .filter(|id| seen.insert(*id))
            .collect();
        if track_ids.is_empty() {
            continue;
        }

        match playlist_repo.create(&name, Some("Importée au scan"), DEFAULT_PROFILE_ID) {
            Ok(pid) => match playlist_repo.add_tracks_deduped(pid, &track_ids, None) {
                Ok(added) => {
                    report.playlists_created += 1;
                    report.tracks_added += added.len();
                    info!(
                        name = %name,
                        matched = track_ids.len(),
                        total = imported.len(),
                        "playlist_scan_imported"
                    );
                }
                Err(e) => warn!(name = %name, error = %e, "playlist_scan_add_tracks_failed"),
            },
            Err(e) => warn!(name = %name, error = %e, "playlist_scan_create_failed"),
        }
    }

    if report.playlists_created > 0 {
        info!(
            playlists = report.playlists_created,
            tracks = report.tracks_added,
            "playlist_scan_complete"
        );
    }
    report
}

/// Walk the scan dirs for `.m3u` / `.m3u8` / `.pls` files.
fn find_playlist_files(dirs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_lowercase().as_str(), "m3u" | "m3u8" | "pls"))
            {
                out.push(path.to_path_buf());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_and_absolute_and_url() {
        let dir = Path::new("/music/My Playlist");
        // Relative → joined against the playlist dir, `..` collapsed.
        assert_eq!(
            resolve_entry("01 - Song.flac", dir).as_deref(),
            Some("/music/My Playlist/01 - Song.flac")
        );
        assert_eq!(
            resolve_entry("../Album/02 - T.flac", dir).as_deref(),
            Some("/music/Album/02 - T.flac")
        );
        // Absolute kept as-is (slash-normalised).
        assert_eq!(
            resolve_entry("/music/x/y.flac", dir).as_deref(),
            Some("/music/x/y.flac")
        );
        // Backslash entry normalised to slashes.
        assert_eq!(
            resolve_entry(r"sub\z.flac", dir).as_deref(),
            Some("/music/My Playlist/sub/z.flac")
        );
        // URLs / empties dropped.
        assert_eq!(resolve_entry("http://radio.example/stream", dir), None);
        assert_eq!(resolve_entry("   ", dir), None);
    }
}
