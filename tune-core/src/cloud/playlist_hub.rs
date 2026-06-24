use std::sync::Arc;

use tracing::{debug, info};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::playlist_repo::PlaylistRepo;

const HUB_API: &str = "https://mozaiklabs.fr/api/v1/premium/playlists";

/// Backup a local playlist to the mozaiklabs cloud Playlist Hub.
///
/// Loads the playlist and its tracks from the local DB, enriches them
/// with service-specific IDs from `track_source_links`, and uploads
/// the full payload to the cloud API.
///
/// Returns the cloud `hub_id` on success.
pub async fn backup_playlist(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
    playlist_id: i64,
) -> Result<String, String> {
    let repo = PlaylistRepo::with_backend(backend.clone());

    // Load playlist metadata
    let playlist = repo
        .get(playlist_id)
        .map_err(|e| format!("load playlist: {e}"))?
        .ok_or_else(|| format!("playlist {playlist_id} not found"))?;

    // Load track IDs
    let track_ids = repo
        .get_track_ids(playlist_id)
        .map_err(|e| format!("load track ids: {e}"))?;

    if track_ids.is_empty() {
        return Err("playlist has no tracks".into());
    }

    // Build track data with service IDs
    let mut tracks = Vec::with_capacity(track_ids.len());
    for tid in &track_ids {
        let track_row = backend
            .query_one(
                "SELECT t.title, t.artist_name, t.album_title, t.isrc, \
                 t.musicbrainz_recording_id, t.duration_ms, t.source, t.source_id \
                 FROM tracks t WHERE t.id = ?",
                &[tid as &dyn ToSqlValue],
            )
            .map_err(|e| format!("load track {tid}: {e}"))?;

        let cols = match track_row {
            Some(c) => c,
            None => {
                debug!(track_id = tid, "playlist_hub_skip_missing_track");
                continue;
            }
        };

        let title = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
        let artist_name = cols.get(1).and_then(|v| v.as_string());
        let album_title = cols.get(2).and_then(|v| v.as_string());
        let isrc = cols.get(3).and_then(|v| v.as_string());
        let mb_recording = cols.get(4).and_then(|v| v.as_string());
        let duration_ms = cols.get(5).and_then(|v| v.as_i64());
        let source = cols.get(6).and_then(|v| v.as_string()).unwrap_or_default();
        let source_id = cols.get(7).and_then(|v| v.as_string());

        // Load service-specific IDs from track_source_links
        let links = backend
            .query_many(
                "SELECT service, service_track_id FROM track_source_links WHERE track_id = ?",
                &[tid as &dyn ToSqlValue],
            )
            .unwrap_or_default();

        let mut qobuz_id: Option<String> = None;
        let mut tidal_id: Option<String> = None;
        let mut spotify_id: Option<String> = None;
        let mut deezer_id: Option<String> = None;
        let mut youtube_id: Option<String> = None;

        for link in &links {
            let svc = link.first().and_then(|v| v.as_string()).unwrap_or_default();
            let sid = link.get(1).and_then(|v| v.as_string());
            match svc.as_str() {
                "qobuz" => qobuz_id = sid,
                "tidal" => tidal_id = sid,
                "spotify" => spotify_id = sid,
                "deezer" => deezer_id = sid,
                "youtube" => youtube_id = sid,
                _ => {}
            }
        }

        // If the track's own source is a streaming service, set that ID too
        match source.as_str() {
            "qobuz" if qobuz_id.is_none() => qobuz_id = source_id.clone(),
            "tidal" if tidal_id.is_none() => tidal_id = source_id.clone(),
            "spotify" if spotify_id.is_none() => spotify_id = source_id.clone(),
            "deezer" if deezer_id.is_none() => deezer_id = source_id.clone(),
            "youtube" if youtube_id.is_none() => youtube_id = source_id.clone(),
            _ => {}
        }

        let mut track_json = serde_json::json!({
            "title": title,
        });
        let obj = track_json.as_object_mut().unwrap();
        if let Some(v) = &artist_name {
            obj.insert("artist_name".into(), serde_json::json!(v));
        }
        if let Some(v) = &album_title {
            obj.insert("album_title".into(), serde_json::json!(v));
        }
        if let Some(v) = &isrc {
            obj.insert("isrc".into(), serde_json::json!(v));
        }
        if let Some(v) = &mb_recording {
            obj.insert("musicbrainz_recording_id".into(), serde_json::json!(v));
        }
        if let Some(v) = duration_ms {
            obj.insert("duration_ms".into(), serde_json::json!(v));
        }
        if let Some(v) = &qobuz_id {
            obj.insert("qobuz_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &tidal_id {
            obj.insert("tidal_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &spotify_id {
            obj.insert("spotify_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &deezer_id {
            obj.insert("deezer_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &youtube_id {
            obj.insert("youtube_id".into(), serde_json::json!(v));
        }

        tracks.push(track_json);
    }

    if tracks.is_empty() {
        return Err("no valid tracks to backup".into());
    }

    let body = serde_json::json!({
        "instance_id": instance_id,
        "name": playlist.name,
        "description": playlist.description,
        "tracks": tracks,
    });

    let resp = http_client
        .post(HUB_API)
        .json(&body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("playlist hub upload: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("playlist hub upload: HTTP {status} — {text}"));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let hub_id = result["hub_id"].as_str().unwrap_or_default().to_string();
    let track_count = result["track_count"].as_i64().unwrap_or(0);

    info!(
        hub_id = %hub_id,
        track_count = track_count,
        playlist = %playlist.name,
        "playlist_hub_backup_done"
    );

    Ok(hub_id)
}

/// List cloud playlists stored in the Playlist Hub for this instance.
pub async fn list_cloud_playlists(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http_client
        .get(HUB_API)
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub list: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("playlist hub list: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let playlists = data["playlists"].as_array().cloned().unwrap_or_default();
    info!(count = playlists.len(), "playlist_hub_list_fetched");
    Ok(playlists)
}

/// Get a single cloud playlist with its full track listing.
pub async fn get_cloud_playlist(
    http_client: &reqwest::Client,
    hub_id: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("{HUB_API}/{hub_id}");
    let resp = http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub get: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("playlist hub get: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    Ok(data)
}

/// Delete a cloud playlist from the Playlist Hub.
pub async fn delete_cloud_playlist(
    http_client: &reqwest::Client,
    hub_id: &str,
) -> Result<(), String> {
    let url = format!("{HUB_API}/{hub_id}");
    let resp = http_client
        .delete(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub delete: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("playlist hub delete: HTTP {}", resp.status()));
    }

    info!(hub_id, "playlist_hub_deleted");
    Ok(())
}

/// Request a playlist transfer to another streaming service.
/// This creates a transfer record on the cloud side. The actual matching
/// and playlist creation happens on the Tune server side (it has the
/// streaming auth tokens).
pub async fn request_transfer(
    http_client: &reqwest::Client,
    instance_id: &str,
    hub_id: &str,
    target_service: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("{HUB_API}/{hub_id}/transfer");
    let body = serde_json::json!({
        "instance_id": instance_id,
        "target_service": target_service,
    });

    let resp = http_client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub transfer: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("playlist hub transfer: HTTP {status} — {text}"));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    info!(
        transfer_id = %result["transfer_id"],
        target = target_service,
        hub_id,
        "playlist_hub_transfer_requested"
    );
    Ok(result)
}
