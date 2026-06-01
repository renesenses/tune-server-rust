use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncLink {
    pub id: Option<i64>,
    pub local_playlist_id: i64,
    pub service: String,
    pub remote_playlist_id: String,
    pub direction: SyncDirection,
    pub last_synced: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncDirection {
    Pull,
    Push,
    Bidirectional,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackSnapshot {
    pub title: String,
    pub artist_name: String,
    pub source_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncDelta {
    pub added: Vec<TrackSnapshot>,
    pub removed: Vec<TrackSnapshot>,
}

pub fn track_key(title: &str, artist: &str) -> String {
    let t = normalize(title);
    let a = normalize(artist);
    format!("{t}|{a}")
}

pub fn normalize(text: &str) -> String {
    let lower = text.to_lowercase();
    let stripped = strip_suffixes(&lower);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_suffixes(text: &str) -> String {
    let mut result = text.to_string();
    let patterns = [
        "(remastered",
        "(remaster",
        "[remastered",
        "[remaster",
        "(deluxe",
        "[deluxe",
        "(live)",
        "[live]",
        "(bonus track)",
        "(mono)",
        "(stereo)",
    ];
    for pat in patterns {
        if let Some(pos) = result.to_lowercase().find(pat) {
            result.truncate(pos);
        }
    }
    result.trim().to_string()
}

pub fn compute_delta(current: &[TrackSnapshot], previous: &[TrackSnapshot]) -> SyncDelta {
    let prev_keys: HashSet<String> = previous
        .iter()
        .map(|t| track_key(&t.title, &t.artist_name))
        .collect();

    let curr_keys: HashSet<String> = current
        .iter()
        .map(|t| track_key(&t.title, &t.artist_name))
        .collect();

    let added: Vec<TrackSnapshot> = current
        .iter()
        .filter(|t| !prev_keys.contains(&track_key(&t.title, &t.artist_name)))
        .cloned()
        .collect();

    let removed: Vec<TrackSnapshot> = previous
        .iter()
        .filter(|t| !curr_keys.contains(&track_key(&t.title, &t.artist_name)))
        .cloned()
        .collect();

    SyncDelta { added, removed }
}

pub fn merge_deltas(
    local_delta: &SyncDelta,
    remote_delta: &SyncDelta,
) -> (Vec<TrackSnapshot>, Vec<TrackSnapshot>) {
    let remote_removed_keys: HashSet<String> = remote_delta
        .removed
        .iter()
        .map(|t| track_key(&t.title, &t.artist_name))
        .collect();

    let local_removed_keys: HashSet<String> = local_delta
        .removed
        .iter()
        .map(|t| track_key(&t.title, &t.artist_name))
        .collect();

    let to_add_locally: Vec<TrackSnapshot> = remote_delta
        .added
        .iter()
        .filter(|t| !local_removed_keys.contains(&track_key(&t.title, &t.artist_name)))
        .cloned()
        .collect();

    let to_add_remotely: Vec<TrackSnapshot> = local_delta
        .added
        .iter()
        .filter(|t| !remote_removed_keys.contains(&track_key(&t.title, &t.artist_name)))
        .cloned()
        .collect();

    (to_add_locally, to_add_remotely)
}

pub struct SyncLinkRepo {
    db: SqliteDb,
}

impl SyncLinkRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn save_link(&self, link: &SyncLink) -> Result<i64, String> {
        let dir = serde_json::to_string(&link.direction).map_err(|e| e.to_string())?;
        let conn = self.db.connection().lock().unwrap();

        if let Some(id) = link.id {
            conn.execute(
                "UPDATE sync_links SET local_playlist_id=?, service=?, remote_playlist_id=?, direction=?, last_synced=? WHERE id=?",
                rusqlite::params![link.local_playlist_id, link.service, link.remote_playlist_id, dir, link.last_synced, id],
            ).map_err(|e| e.to_string())?;
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO sync_links (local_playlist_id, service, remote_playlist_id, direction, last_synced) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![link.local_playlist_id, link.service, link.remote_playlist_id, dir, link.last_synced],
            ).map_err(|e| e.to_string())?;
            Ok(conn.last_insert_rowid())
        }
    }

    pub fn save_snapshot(
        &self,
        link_id: i64,
        side: &str,
        tracks: &[TrackSnapshot],
    ) -> Result<(), String> {
        let json = serde_json::to_string(tracks).map_err(|e| e.to_string())?;
        let now = chrono::Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT INTO sync_link_snapshots (playlist_link_id, side, tracks_json, created_at) VALUES (?, ?, ?, ?)",
            &[
                &link_id as &dyn rusqlite::types::ToSql,
                &side,
                &json,
                &now,
            ],
        )?;
        Ok(())
    }

    pub fn load_last_snapshot(
        &self,
        link_id: i64,
        side: &str,
    ) -> Result<Option<Vec<TrackSnapshot>>, String> {
        let conn = self.db.connection().lock().unwrap();
        let result = conn.query_row(
            "SELECT tracks_json FROM sync_link_snapshots WHERE playlist_link_id = ? AND side = ? ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![link_id, side],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(json) => {
                let tracks: Vec<TrackSnapshot> =
                    serde_json::from_str(&json).map_err(|e| e.to_string())?;
                Ok(Some(tracks))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn list_links(&self) -> Result<Vec<SyncLink>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, local_playlist_id, service, remote_playlist_id, direction, last_synced FROM sync_links ORDER BY id")
            .map_err(|e| e.to_string())?;

        let links = stmt
            .query_map([], |row| {
                let dir_str: String = row.get(4)?;
                let direction =
                    serde_json::from_str(&dir_str).unwrap_or(SyncDirection::Bidirectional);
                Ok(SyncLink {
                    id: row.get(0).ok(),
                    local_playlist_id: row.get(1)?,
                    service: row.get(2)?,
                    remote_playlist_id: row.get(3)?,
                    direction,
                    last_synced: row.get(5).ok().flatten(),
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(links)
    }

    pub fn delete_link(&self, id: i64) -> Result<(), String> {
        self.db
            .execute("DELETE FROM sync_links WHERE id = ?", &[&id])?;
        self.db.execute(
            "DELETE FROM sync_link_snapshots WHERE playlist_link_id = ?",
            &[&id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(title: &str, artist: &str) -> TrackSnapshot {
        TrackSnapshot {
            title: title.into(),
            artist_name: artist.into(),
            source_id: String::new(),
        }
    }

    #[test]
    fn normalize_basic() {
        assert_eq!(normalize("Hello World"), "hello world");
        assert_eq!(normalize("  spaces  "), "spaces");
    }

    #[test]
    fn normalize_strips_remaster() {
        assert_eq!(
            normalize("Bohemian Rhapsody (Remastered 2011)"),
            "bohemian rhapsody"
        );
    }

    #[test]
    fn track_key_format() {
        let key = track_key("Song", "Artist");
        assert_eq!(key, "song|artist");
    }

    #[test]
    fn compute_delta_additions() {
        let prev = vec![snap("A", "X")];
        let curr = vec![snap("A", "X"), snap("B", "Y")];
        let delta = compute_delta(&curr, &prev);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].title, "B");
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn compute_delta_removals() {
        let prev = vec![snap("A", "X"), snap("B", "Y")];
        let curr = vec![snap("A", "X")];
        let delta = compute_delta(&curr, &prev);
        assert!(delta.added.is_empty());
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.removed[0].title, "B");
    }

    #[test]
    fn compute_delta_no_change() {
        let tracks = vec![snap("A", "X")];
        let delta = compute_delta(&tracks, &tracks);
        assert!(delta.added.is_empty());
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn merge_deltas_bidirectional() {
        let local_delta = SyncDelta {
            added: vec![snap("New Local", "Artist")],
            removed: vec![],
        };
        let remote_delta = SyncDelta {
            added: vec![snap("New Remote", "Artist")],
            removed: vec![],
        };

        let (to_local, to_remote) = merge_deltas(&local_delta, &remote_delta);
        assert_eq!(to_local.len(), 1);
        assert_eq!(to_local[0].title, "New Remote");
        assert_eq!(to_remote.len(), 1);
        assert_eq!(to_remote[0].title, "New Local");
    }

    #[test]
    fn merge_respects_removals() {
        let local_delta = SyncDelta {
            added: vec![],
            removed: vec![snap("Removed", "X")],
        };
        let remote_delta = SyncDelta {
            added: vec![snap("Removed", "X")],
            removed: vec![],
        };

        let (to_local, _) = merge_deltas(&local_delta, &remote_delta);
        assert!(to_local.is_empty());
    }
}
