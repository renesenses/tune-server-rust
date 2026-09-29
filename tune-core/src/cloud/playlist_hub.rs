use std::sync::Arc;

use tracing::{debug, info};

use crate::cloud::refusal::CloudError;
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
) -> Result<String, CloudError> {
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
        match reference_de_piste(backend, *tid)? {
            Some(reference) => tracks.push(reference),
            None => debug!(track_id = tid, "playlist_hub_skip_missing_track"),
        }
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
        let entetes = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        return Err(CloudError::from_parts(
            format!("playlist hub upload: HTTP {status} — {text}"),
            status.as_u16(),
            &entetes,
            &text,
        ));
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

/// Les identifiants de service qu'une référence peut porter, dans l'ordre où
/// [`reference_de_piste`] les écrit.
pub const SERVICES_DE_REFERENCE: [&str; 5] = ["qobuz", "tidal", "spotify", "deezer", "youtube"];

/// La RÉFÉRENCE d'une piste de la base : ce qui la désigne hors de ce serveur,
/// sans rien qui n'ait de sens qu'ici.
///
/// `title`, puis, s'ils sont connus, `artist_name`, `album_title`, `isrc`,
/// `musicbrainz_recording_id`, `duration_ms` et `{service}_id` pour chaque
/// service de [`SERVICES_DE_REFERENCE`]. Les identifiants de service viennent
/// de `track_source_links`, ou de la source de la piste elle-même quand elle
/// est un titre de ce service.
///
/// Jamais `file_path`, `source_id` d'une piste locale, `cover_path` ni
/// l'identifiant de ligne : la colonne n'est même pas lue. C'est le format du
/// Playlist Hub, réemployé tel quel par les playlists de cercle (#5328).
///
/// `Ok(None)` pour un identifiant sans ligne dans `tracks`.
pub fn reference_de_piste(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
) -> Result<Option<serde_json::Value>, String> {
    let tid = &track_id;
    let track_row = backend
        .query_one(
            // `tracks` n'a NI `artist_name` NI `album_title` : ils viennent
            // de `artists` et d'`albums`, comme dans `TrackRepo`. L'ancienne
            // lecture de `t.artist_name` échouait sur toute piste (#5328).
            "SELECT t.title, ar.name, al.title, t.isrc, \
             t.musicbrainz_recording_id, t.duration_ms, t.source, t.source_id \
             FROM tracks t \
             LEFT JOIN artists ar ON ar.id = t.artist_id \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE t.id = ?",
            &[tid as &dyn ToSqlValue],
        )
        .map_err(|e| format!("load track {tid}: {e}"))?;

    let Some(cols) = track_row else {
        return Ok(None);
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

    let mut ids: [Option<String>; 5] = Default::default();
    for link in &links {
        let svc = link.first().and_then(|v| v.as_string()).unwrap_or_default();
        let sid = link.get(1).and_then(|v| v.as_string());
        if let Some(i) = SERVICES_DE_REFERENCE.iter().position(|s| *s == svc) {
            ids[i] = sid;
        }
    }

    // If the track's own source is a streaming service, set that ID too.
    // A LOCAL track's `source_id` is never read here: it may be a path.
    if let Some(i) = SERVICES_DE_REFERENCE.iter().position(|s| *s == source)
        && ids[i].is_none()
    {
        ids[i] = source_id.clone();
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
    for (svc, id) in SERVICES_DE_REFERENCE.iter().zip(ids) {
        if let Some(v) = id {
            obj.insert(format!("{svc}_id"), serde_json::json!(v));
        }
    }
    Ok(Some(track_json))
}

/// List cloud playlists stored in the Playlist Hub for this instance.
pub async fn list_cloud_playlists(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, CloudError> {
    let resp = http_client
        .get(HUB_API)
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub list: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(
            CloudError::from_response(format!("playlist hub list: HTTP {status}"), resp).await,
        );
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
) -> Result<serde_json::Value, CloudError> {
    let url = format!("{HUB_API}/{hub_id}");
    let resp = http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub get: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(
            CloudError::from_response(format!("playlist hub get: HTTP {status}"), resp).await,
        );
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    Ok(data)
}

/// Delete a cloud playlist from the Playlist Hub.
pub async fn delete_cloud_playlist(
    http_client: &reqwest::Client,
    hub_id: &str,
) -> Result<(), CloudError> {
    let url = format!("{HUB_API}/{hub_id}");
    let resp = http_client
        .delete(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("playlist hub delete: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(
            CloudError::from_response(format!("playlist hub delete: HTTP {status}"), resp).await,
        );
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
) -> Result<serde_json::Value, CloudError> {
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
        let entetes = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        return Err(CloudError::from_parts(
            format!("playlist hub transfer: HTTP {status} — {text}"),
            status.as_u16(),
            &entetes,
            &text,
        ));
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
