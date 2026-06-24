use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};

const CLOUD_LIBRARY_API: &str = "https://mozaiklabs.fr/api/v1/cloud-library";
const SYNC_BATCH_SIZE: i64 = 200;

// ---------------------------------------------------------------------------
// SyncReport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub tracks_synced: i64,
    pub albums_synced: i64,
    pub artists_synced: i64,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// record_change — fire-and-forget changelog insertion
// ---------------------------------------------------------------------------

/// Insert a changelog entry so the next sync push picks up this entity.
/// entity_type: "track", "album", "artist", "playlist", "favorite", "rating", "history".
/// action: "upsert" or "delete".
///
/// This is fire-and-forget — it never fails the caller.
pub fn record_change(
    backend: &Arc<dyn DbBackend>,
    entity_type: &str,
    entity_id: i64,
    action: &str,
) {
    backend
        .execute(
            "INSERT INTO sync_changelog (entity_type, entity_id, action) VALUES (?, ?, ?)",
            &[
                &entity_type.to_string() as &dyn ToSqlValue,
                &entity_id as &dyn ToSqlValue,
                &action.to_string() as &dyn ToSqlValue,
            ],
        )
        .ok(); // fire-and-forget
}

// ---------------------------------------------------------------------------
// pending_count
// ---------------------------------------------------------------------------

/// Count unsynced changelog entries.
pub fn pending_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one("SELECT COUNT(*) FROM sync_changelog WHERE synced = 0", &[])
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// push_changes — incremental sync
// ---------------------------------------------------------------------------

/// Read unsynced changelog entries, load entity data, POST to cloud API,
/// and mark entries as synced.  Returns a report.
pub async fn push_changes(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
) -> Result<SyncReport, String> {
    let start = Instant::now();
    let mut report = SyncReport {
        tracks_synced: 0,
        albums_synced: 0,
        artists_synced: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    loop {
        // 1. Read a batch of unsynced changelog entries
        let batch_limit = SYNC_BATCH_SIZE;
        let rows = backend
            .query_many(
                "SELECT id, entity_type, entity_id, action FROM sync_changelog \
                 WHERE synced = 0 ORDER BY changed_at ASC LIMIT ?",
                &[&batch_limit as &dyn ToSqlValue],
            )
            .map_err(|e| format!("changelog query: {e}"))?;

        if rows.is_empty() {
            break;
        }

        // Collect changelog entries
        let mut entries: Vec<(i64, String, i64, String)> = Vec::new();
        for row in &rows {
            let id = row.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            let etype = row.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let eid = row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
            let action = row.get(3).and_then(|v| v.as_string()).unwrap_or_default();
            entries.push((id, etype, eid, action));
        }

        // 2. Group by entity_type
        let mut track_ids: Vec<i64> = Vec::new();
        let mut album_ids: Vec<i64> = Vec::new();
        let mut artist_ids: Vec<i64> = Vec::new();
        let mut changelog_ids: Vec<i64> = Vec::new();
        let mut changes: Vec<serde_json::Value> = Vec::new();

        for (cl_id, etype, eid, action) in &entries {
            changelog_ids.push(*cl_id);
            match etype.as_str() {
                "track" => track_ids.push(*eid),
                "album" => album_ids.push(*eid),
                "artist" => artist_ids.push(*eid),
                _ => {
                    // For non-entity types (playlist, favorite, rating, history),
                    // just record the change with no data payload
                    changes.push(serde_json::json!({
                        "type": etype,
                        "action": action,
                        "id": eid,
                        "data": null,
                    }));
                }
            }
        }

        // 3. Load full entity data for tracks
        if !track_ids.is_empty() {
            let placeholders = track_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT t.id, t.title, ar.name, al.title, t.format, t.sample_rate, t.bit_depth, \
                 t.duration_ms, t.genre, t.track_number, t.disc_number, t.source, t.source_id \
                 FROM tracks t \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE t.id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                track_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(trows) = backend.query_many(&sql, &params) {
                for r in &trows {
                    let tid = r.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "track" && *eid == tid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "track",
                        "action": action,
                        "id": tid,
                        "data": {
                            "title": r.get(1).and_then(|v| v.as_string()),
                            "artist_name": r.get(2).and_then(|v| v.as_string()),
                            "album_title": r.get(3).and_then(|v| v.as_string()),
                            "format": r.get(4).and_then(|v| v.as_string()),
                            "sample_rate": r.get(5).and_then(|v| v.as_i64()),
                            "bit_depth": r.get(6).and_then(|v| v.as_i64()),
                            "duration_ms": r.get(7).and_then(|v| v.as_i64()),
                            "genre": r.get(8).and_then(|v| v.as_string()),
                            "track_number": r.get(9).and_then(|v| v.as_i64()),
                            "disc_number": r.get(10).and_then(|v| v.as_i64()),
                            "source": r.get(11).and_then(|v| v.as_string()),
                            "source_id": r.get(12).and_then(|v| v.as_string()),
                        }
                    }));
                    report.tracks_synced += 1;
                }
            }
            // Handle deletes — tracks that no longer exist in DB
            for tid in &track_ids {
                let already_in_changes = changes
                    .iter()
                    .any(|c| c["type"] == "track" && c["id"].as_i64() == Some(*tid));
                if !already_in_changes {
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "track" && *eid == *tid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("delete");
                    changes.push(serde_json::json!({
                        "type": "track",
                        "action": action,
                        "id": tid,
                        "data": null,
                    }));
                    report.tracks_synced += 1;
                }
            }
        }

        // 4. Load full entity data for albums
        if !album_ids.is_empty() {
            let placeholders = album_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT a.id, a.title, ar.name, a.year, a.genre, a.source, \
                 (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) AS track_count \
                 FROM albums a \
                 LEFT JOIN artists ar ON a.artist_id = ar.id \
                 WHERE a.id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                album_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(arows) = backend.query_many(&sql, &params) {
                for r in &arows {
                    let aid = r.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "album" && *eid == aid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "album",
                        "action": action,
                        "id": aid,
                        "data": {
                            "title": r.get(1).and_then(|v| v.as_string()),
                            "artist_name": r.get(2).and_then(|v| v.as_string()),
                            "year": r.get(3).and_then(|v| v.as_i64()),
                            "genre": r.get(4).and_then(|v| v.as_string()),
                            "source": r.get(5).and_then(|v| v.as_string()),
                            "track_count": r.get(6).and_then(|v| v.as_i64()),
                        }
                    }));
                    report.albums_synced += 1;
                }
            }
            for aid in &album_ids {
                let already = changes
                    .iter()
                    .any(|c| c["type"] == "album" && c["id"].as_i64() == Some(*aid));
                if !already {
                    changes.push(serde_json::json!({
                        "type": "album",
                        "action": "delete",
                        "id": aid,
                        "data": null,
                    }));
                    report.albums_synced += 1;
                }
            }
        }

        // 5. Load full entity data for artists
        if !artist_ids.is_empty() {
            let placeholders = artist_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, name, bio, musicbrainz_id FROM artists WHERE id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                artist_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(rrows) = backend.query_many(&sql, &params) {
                for r in &rrows {
                    let rid = r.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "artist" && *eid == rid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "artist",
                        "action": action,
                        "id": rid,
                        "data": {
                            "name": r.get(1).and_then(|v| v.as_string()),
                            "bio": r.get(2).and_then(|v| v.as_string()),
                            "musicbrainz_id": r.get(3).and_then(|v| v.as_string()),
                        }
                    }));
                    report.artists_synced += 1;
                }
            }
            for rid in &artist_ids {
                let already = changes
                    .iter()
                    .any(|c| c["type"] == "artist" && c["id"].as_i64() == Some(*rid));
                if !already {
                    changes.push(serde_json::json!({
                        "type": "artist",
                        "action": "delete",
                        "id": rid,
                        "data": null,
                    }));
                    report.artists_synced += 1;
                }
            }
        }

        // 6. POST batch to cloud API
        if !changes.is_empty() {
            let payload = serde_json::json!({
                "server_id": server_id,
                "changes": changes,
            });

            match http_client
                .post(format!("{CLOUD_LIBRARY_API}/{server_id}/sync"))
                .bearer_auth(access_token)
                .json(&payload)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    debug!(
                        batch_size = changes.len(),
                        "cloud_library_sync_batch_pushed"
                    );
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let msg = format!("cloud sync HTTP {status}: {body}");
                    warn!(error = %msg, "cloud_library_sync_batch_failed");
                    report.errors.push(msg);
                    // Don't mark as synced on failure — will retry next cycle
                    break;
                }
                Err(e) => {
                    let msg = format!("cloud sync request: {e}");
                    warn!(error = %msg, "cloud_library_sync_request_failed");
                    report.errors.push(msg);
                    break;
                }
            }
        }

        // 7. Mark changelog entries as synced
        if !changelog_ids.is_empty() {
            let placeholders = changelog_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("UPDATE sync_changelog SET synced = 1 WHERE id IN ({placeholders})");
            let params: Vec<&dyn ToSqlValue> = changelog_ids
                .iter()
                .map(|id| id as &dyn ToSqlValue)
                .collect();
            backend.execute(&sql, &params).ok();
        }
    }

    report.duration_ms = start.elapsed().as_millis() as u64;
    Ok(report)
}

// ---------------------------------------------------------------------------
// full_sync — push entire library
// ---------------------------------------------------------------------------

/// Queue the entire library for cloud sync by inserting all tracks, albums,
/// and artists into sync_changelog as "upsert", then push changes in a loop.
pub async fn full_sync(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
) -> Result<SyncReport, String> {
    info!("cloud_library_full_sync_starting");

    // Bulk-insert all entities that aren't already pending
    backend
        .execute_batch(
            "INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'track', id, 'upsert' FROM tracks \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='track' AND synced=0);\
             INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'album', id, 'upsert' FROM albums \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='album' AND synced=0);\
             INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'artist', id, 'upsert' FROM artists \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='artist' AND synced=0);",
        )
        .map_err(|e| format!("full_sync bulk insert: {e}"))?;

    let total_pending = pending_count(backend);
    info!(pending = total_pending, "cloud_library_full_sync_queued");

    // Push in batches until done
    let mut combined = SyncReport {
        tracks_synced: 0,
        albums_synced: 0,
        artists_synced: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };
    let start = Instant::now();

    loop {
        let remaining = pending_count(backend);
        if remaining == 0 {
            break;
        }

        match push_changes(backend, http_client, server_id, access_token).await {
            Ok(batch_report) => {
                combined.tracks_synced += batch_report.tracks_synced;
                combined.albums_synced += batch_report.albums_synced;
                combined.artists_synced += batch_report.artists_synced;
                combined.errors.extend(batch_report.errors);
            }
            Err(e) => {
                combined.errors.push(e);
                break;
            }
        }
    }

    combined.duration_ms = start.elapsed().as_millis() as u64;
    info!(
        tracks = combined.tracks_synced,
        albums = combined.albums_synced,
        artists = combined.artists_synced,
        errors = combined.errors.len(),
        duration_ms = combined.duration_ms,
        "cloud_library_full_sync_complete"
    );
    Ok(combined)
}

// ---------------------------------------------------------------------------
// populate_changelog_after_scan — bulk changelog population
// ---------------------------------------------------------------------------

/// After a library scan completes, bulk-insert changelog entries for all
/// tracks, albums, and artists that don't already have a pending entry.
/// This is more efficient than instrumenting every individual insert.
pub fn populate_changelog_after_scan(backend: &Arc<dyn DbBackend>) {
    let result = backend.execute_batch(
        "INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'track', id, 'upsert' FROM tracks \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='track' AND synced=0);\
         INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'album', id, 'upsert' FROM albums \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='album' AND synced=0);\
         INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'artist', id, 'upsert' FROM artists \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='artist' AND synced=0);",
    );
    match result {
        Ok(()) => info!("sync_changelog_populated_after_scan"),
        Err(e) => warn!(error = %e, "sync_changelog_populate_failed"),
    }
}

// ---------------------------------------------------------------------------
// spawn — background sync task
// ---------------------------------------------------------------------------

/// Spawn the periodic cloud library sync task.  Runs every 5 minutes,
/// gated behind Premium tier + SSO access token.
pub fn spawn(backend: Arc<dyn DbBackend>, license: Arc<crate::license::LicenseManager>) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "cloud_library_sync_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 2 minutes after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            // Only sync if premium
            if license.is_premium().await {
                let settings =
                    crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
                let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
                let token = settings.get("mozaik_access_token").ok().flatten();

                if let Some(token) = token {
                    if !server_id.is_empty() {
                        let pending = pending_count(&backend);
                        if pending > 0 {
                            info!(pending, "cloud_library_sync_starting");
                            match push_changes(&backend, &client, &server_id, &token).await {
                                Ok(report) => {
                                    // Store last sync time
                                    let now = chrono::Utc::now().to_rfc3339();
                                    settings.set("cloud_library_last_sync", &now).ok();
                                    info!(
                                        tracks = report.tracks_synced,
                                        albums = report.albums_synced,
                                        artists = report.artists_synced,
                                        errors = report.errors.len(),
                                        duration_ms = report.duration_ms,
                                        "cloud_library_sync_complete"
                                    );
                                }
                                Err(e) => {
                                    warn!(error = %e, "cloud_library_sync_failed");
                                }
                            }
                        }
                    } else {
                        debug!("cloud_library_sync_skipped_no_server_id");
                    }
                }
            }

            // Every 5 minutes
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}
