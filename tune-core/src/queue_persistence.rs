//! Queue persistence: save/restore zone queues to JSON files so they survive
//! server restarts. Files are stored alongside the database (same parent dir).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::playback::ZoneState;

/// Snapshot of a zone's queue state, serialized to JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub zone_id: i64,
    /// Local track IDs in queue order (from play_queue table).
    pub local_track_ids: Vec<i64>,
    /// Current track position index.
    pub current_position: i64,
    /// Streaming queue items (for Tidal/Qobuz/Deezer/etc).
    pub streaming_tracks: Vec<StreamingQueueEntry>,
    /// Repeat mode: "off", "one", or "all".
    pub repeat_mode: String,
    /// Whether shuffle is enabled.
    pub shuffle: bool,
    /// Playback position in milliseconds within the current track.
    pub position_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamingQueueEntry {
    pub source_id: String,
    pub title: String,
    pub artist_name: String,
    pub album_title: Option<String>,
    pub cover_url: Option<String>,
    pub duration_ms: i64,
    /// Streaming service name (e.g. "tidal", "qobuz", "deezer").
    #[serde(default)]
    pub source: Option<String>,
}

/// Directory where queue files are stored (same parent as the DB file).
fn queue_dir(db_path: &str) -> PathBuf {
    let db = Path::new(db_path);
    let parent = db.parent().unwrap_or(Path::new("."));
    parent.join("queue_state")
}

fn queue_file_path(db_path: &str, zone_id: i64) -> PathBuf {
    queue_dir(db_path).join(format!("queue_{zone_id}.json"))
}

/// Save the current queue state for a zone to a JSON file.
pub fn save_queue(db: &Arc<dyn DbBackend>, db_path: &str, zone_id: i64, zone_state: &ZoneState) {
    let dir = queue_dir(db_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(zone_id, error = %e, "queue_persist_mkdir_failed");
        return;
    }

    let repo = PlayQueueRepo::with_backend(db.clone());

    // Gather local queue track IDs
    let local_items = repo.get_queue(zone_id).unwrap_or_default();
    let local_track_ids: Vec<i64> = local_items.iter().map(|i| i.track_id).collect();
    let current_position = local_items
        .iter()
        .position(|i| i.is_current)
        .map(|p| p as i64)
        .unwrap_or(zone_state.queue_position);

    // Gather streaming queue
    let streaming_items = repo.get_streaming_queue(zone_id).unwrap_or_default();
    let streaming_tracks: Vec<StreamingQueueEntry> = streaming_items
        .iter()
        .map(|item| StreamingQueueEntry {
            source_id: item["source_id"].as_str().unwrap_or_default().to_string(),
            title: item["title"].as_str().unwrap_or_default().to_string(),
            artist_name: item["artist_name"].as_str().unwrap_or_default().to_string(),
            album_title: item["album_title"].as_str().map(String::from),
            cover_url: item["cover_path"].as_str().map(String::from),
            duration_ms: item["duration_ms"].as_i64().unwrap_or(0),
            source: item["source"].as_str().map(String::from),
        })
        .collect();

    let repeat_mode = match zone_state.repeat {
        crate::playback::RepeatMode::Off => "off",
        crate::playback::RepeatMode::One => "one",
        crate::playback::RepeatMode::All => "all",
    };

    let snapshot = QueueSnapshot {
        zone_id,
        local_track_ids,
        current_position,
        streaming_tracks,
        repeat_mode: repeat_mode.to_string(),
        shuffle: zone_state.shuffle,
        position_ms: zone_state.position_ms,
    };

    let path = queue_file_path(db_path, zone_id);
    match serde_json::to_string_pretty(&snapshot) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!(zone_id, error = %e, "queue_persist_write_failed");
            }
        }
        Err(e) => {
            warn!(zone_id, error = %e, "queue_persist_serialize_failed");
        }
    }
}

/// Restore all saved queue snapshots from disk and repopulate the DB tables.
/// Called at server startup.
pub fn restore_all_queues(db: &Arc<dyn DbBackend>, db_path: &str) {
    let dir = queue_dir(db_path);
    if !dir.exists() {
        return;
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "queue_restore_readdir_failed");
            return;
        }
    };

    let repo = PlayQueueRepo::with_backend(db.clone());
    let mut restored = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "queue_restore_read_failed");
                continue;
            }
        };

        let snapshot: QueueSnapshot = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "queue_restore_parse_failed");
                continue;
            }
        };

        let zone_id = snapshot.zone_id;

        // Check if DB already has a queue for this zone (don't overwrite)
        let existing = repo.get_queue(zone_id).unwrap_or_default();
        if !existing.is_empty() {
            continue;
        }

        // Restore local queue — filter out track IDs that no longer exist in DB
        if !snapshot.local_track_ids.is_empty() {
            let track_repo = crate::db::track_repo::TrackRepo::with_backend(db.clone());
            let valid_ids: Vec<i64> = snapshot
                .local_track_ids
                .iter()
                .copied()
                .filter(|id| track_repo.get(*id).ok().flatten().is_some())
                .collect();
            if valid_ids.is_empty() {
                debug!(
                    zone_id,
                    original = snapshot.local_track_ids.len(),
                    "queue_restore_all_tracks_gone"
                );
                continue;
            }
            if valid_ids.len() < snapshot.local_track_ids.len() {
                debug!(
                    zone_id,
                    original = snapshot.local_track_ids.len(),
                    valid = valid_ids.len(),
                    "queue_restore_filtered_stale_tracks"
                );
            }
            if let Err(e) = repo.set_queue(zone_id, &valid_ids) {
                warn!(zone_id, error = %e, "queue_restore_set_queue_failed");
                continue;
            }
            if snapshot.current_position > 0 {
                repo.set_current(zone_id, snapshot.current_position).ok();
            }
        }

        // Restore streaming queue
        if !snapshot.streaming_tracks.is_empty() {
            let tracks: Vec<(
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                i64,
                Option<String>,
            )> = snapshot
                .streaming_tracks
                .iter()
                .map(|t| {
                    (
                        t.source_id.clone(),
                        t.title.clone(),
                        t.artist_name.clone(),
                        t.album_title.clone(),
                        t.cover_url.clone(),
                        t.duration_ms,
                        t.source.clone(),
                    )
                })
                .collect();
            if let Err(e) = repo.set_streaming_queue(zone_id, &tracks) {
                warn!(zone_id, error = %e, "queue_restore_streaming_failed");
                continue;
            }
        }

        restored += 1;
        info!(
            zone_id,
            local_tracks = snapshot.local_track_ids.len(),
            streaming_tracks = snapshot.streaming_tracks.len(),
            position = snapshot.current_position,
            "queue_restored"
        );
    }

    if restored > 0 {
        info!(count = restored, "queues_restore_complete");
    }
}

/// Load all queue snapshots from disk without modifying the DB.
/// Used at startup to extract metadata (repeat_mode, shuffle, position, lengths).
pub fn load_all_snapshots(db_path: &str) -> Vec<QueueSnapshot> {
    let dir = queue_dir(db_path);
    if !dir.exists() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut snapshots = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(snapshot) = serde_json::from_str::<QueueSnapshot>(&content) {
                snapshots.push(snapshot);
            }
        }
    }
    snapshots
}

/// Delete the queue snapshot file for a zone (e.g., when the queue is cleared).
pub fn delete_queue_file(db_path: &str, zone_id: i64) {
    let path = queue_file_path(db_path, zone_id);
    if path.exists() {
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use crate::playback::{RepeatMode, ZoneState};

    #[test]
    fn snapshot_roundtrip() {
        let snapshot = QueueSnapshot {
            zone_id: 1,
            local_track_ids: vec![10, 20, 30],
            current_position: 1,
            streaming_tracks: vec![StreamingQueueEntry {
                source_id: "tidal:123".into(),
                title: "Song".into(),
                artist_name: "Artist".into(),
                album_title: Some("Album".into()),
                cover_url: None,
                duration_ms: 200_000,
                source: Some("tidal".into()),
            }],
            repeat_mode: "all".into(),
            shuffle: true,
            position_ms: 45_000,
        };

        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let restored: QueueSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.zone_id, 1);
        assert_eq!(restored.local_track_ids, vec![10, 20, 30]);
        assert_eq!(restored.current_position, 1);
        assert_eq!(restored.streaming_tracks.len(), 1);
        assert_eq!(restored.repeat_mode, "all");
        assert!(restored.shuffle);
        assert_eq!(restored.position_ms, 45_000);
    }

    #[test]
    fn save_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("tune.db");
        let db_path_str = db_path.to_str().unwrap();

        // Create in-memory DB with schema
        let sqlite = SqliteDb::open_in_memory().unwrap();
        sqlite.init_schema().unwrap();
        crate::db::migrations::run_migrations(&sqlite).unwrap();
        let db: std::sync::Arc<dyn crate::db::backend::DbBackend> = std::sync::Arc::new(sqlite);

        // Insert a zone + tracks
        db.execute(
            "INSERT INTO zones (id, name, output_type) VALUES (1, 'Main', 'local')",
            &[],
        )
        .unwrap();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
        for i in 1..=3i64 {
            let title = format!("Track {i}");
            db.execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) VALUES (?, ?, 1, 1, 180000)",
                &[&i as &dyn crate::db::backend::ToSqlValue, &title.as_str()],
            )
            .unwrap();
        }

        // Set up a queue in the DB
        let repo = PlayQueueRepo::with_backend(db.clone());
        repo.set_queue(1, &[1, 2, 3]).unwrap();
        repo.set_current(1, 1).unwrap();

        let zone_state = ZoneState {
            zone_id: 1,
            shuffle: true,
            repeat: RepeatMode::All,
            position_ms: 42_000,
            queue_position: 1,
            queue_length: 3,
            ..Default::default()
        };

        // Save
        save_queue(&db, db_path_str, 1, &zone_state);

        // Verify file exists
        let file = queue_file_path(db_path_str, 1);
        assert!(file.exists(), "queue file should exist");

        // Clear the DB queue
        repo.clear(1).unwrap();
        assert!(repo.get_queue(1).unwrap().is_empty());

        // Restore
        restore_all_queues(&db, db_path_str);

        // Verify queue is back
        let queue = repo.get_queue(1).unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].track_id, 1);
        assert_eq!(queue[1].track_id, 2);
        assert_eq!(queue[2].track_id, 3);

        // Verify current position was restored
        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.position, 1);
    }
}
