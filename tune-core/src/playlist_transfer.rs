use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::streaming::registry::ServiceRegistry;
use crate::streaming::traits::StreamingService;

// ---------------------------------------------------------------------------
// Request / Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct TransferRequest {
    pub source_service: String,
    pub source_playlist_id: String,
    pub target_service: String,
    pub target_playlist_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferReport {
    pub total_tracks: usize,
    pub matched: usize,
    pub not_found: Vec<String>,
    pub target_playlist_id: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewMatch {
    pub source_title: String,
    pub source_artist: String,
    pub target_track_id: Option<String>,
    pub target_title: Option<String>,
    pub target_artist: Option<String>,
    pub status: String, // "matched" | "not_found"
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewReport {
    pub total_tracks: usize,
    pub matched: usize,
    pub not_found: usize,
    pub matches: Vec<PreviewMatch>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferProgress {
    pub status: String,
    pub current: usize,
    pub total: usize,
    pub matched: usize,
    pub not_found: usize,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a service from the registry, returning a usable Arc-wrapped service.
async fn resolve_service(
    registry: &Arc<Mutex<ServiceRegistry>>,
    name: &str,
) -> Result<Arc<Mutex<Box<dyn StreamingService>>>, String> {
    let reg = registry.lock().await;
    reg.get(name)
        .ok_or_else(|| format!("service not found: {name}"))
}

/// Search for a track on the target service. Returns the target track ID if
/// found. We search by "title artist" and pick the first result whose title
/// is close enough.
async fn search_track_on_target(
    target: &Arc<Mutex<Box<dyn StreamingService>>>,
    title: &str,
    artist: &str,
) -> Option<(String, String, String)> {
    let query = format!("{title} {artist}");
    let svc = target.lock().await;
    let results = match svc.search(&query, 5).await {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, query = %query, "transfer_search_failed");
            return None;
        }
    };

    let title_lower = title.to_lowercase();
    let artist_lower = artist.to_lowercase();

    // Best match: exact title + artist
    for track in &results.tracks {
        if track.title.to_lowercase() == title_lower && track.artist.to_lowercase() == artist_lower
        {
            return Some((track.id.clone(), track.title.clone(), track.artist.clone()));
        }
    }

    // Fallback: title contains the source title
    for track in &results.tracks {
        if track.title.to_lowercase().contains(&title_lower)
            || title_lower.contains(&track.title.to_lowercase())
        {
            return Some((track.id.clone(), track.title.clone(), track.artist.clone()));
        }
    }

    // Last resort: just take the first result if any
    if let Some(track) = results.tracks.first() {
        // Only accept if artist has some overlap
        if track.artist.to_lowercase().contains(&artist_lower)
            || artist_lower.contains(&track.artist.to_lowercase())
        {
            return Some((track.id.clone(), track.title.clone(), track.artist.clone()));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Core transfer functions
// ---------------------------------------------------------------------------

/// Preview a transfer: match source tracks on the target but do not create
/// anything.
pub async fn preview_transfer(
    services: &Arc<Mutex<ServiceRegistry>>,
    req: &TransferRequest,
) -> Result<PreviewReport, String> {
    let start = Instant::now();

    // Load source playlist tracks
    let source_svc = resolve_service(services, &req.source_service).await?;
    let source_tracks = {
        let svc = source_svc.lock().await;
        svc.get_playlist_tracks(&req.source_playlist_id).await?
    };

    let total_tracks = source_tracks.len();
    if total_tracks == 0 {
        return Ok(PreviewReport {
            total_tracks: 0,
            matched: 0,
            not_found: 0,
            matches: vec![],
            duration_ms: start.elapsed().as_millis() as u64,
        });
    }

    // Resolve target service
    let target_svc = resolve_service(services, &req.target_service).await?;

    let mut matches = Vec::with_capacity(total_tracks);
    let mut matched = 0usize;
    let mut not_found = 0usize;

    for track in &source_tracks {
        let found = search_track_on_target(&target_svc, &track.title, &track.artist).await;

        match found {
            Some((tid, ttitle, tartist)) => {
                matched += 1;
                matches.push(PreviewMatch {
                    source_title: track.title.clone(),
                    source_artist: track.artist.clone(),
                    target_track_id: Some(tid),
                    target_title: Some(ttitle),
                    target_artist: Some(tartist),
                    status: "matched".into(),
                });
            }
            None => {
                not_found += 1;
                matches.push(PreviewMatch {
                    source_title: track.title.clone(),
                    source_artist: track.artist.clone(),
                    target_track_id: None,
                    target_title: None,
                    target_artist: None,
                    status: "not_found".into(),
                });
            }
        }
    }

    Ok(PreviewReport {
        total_tracks,
        matched,
        not_found,
        matches,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

/// Execute a full playlist transfer: match + create + add tracks.
pub async fn transfer_playlist(
    services: &Arc<Mutex<ServiceRegistry>>,
    req: &TransferRequest,
) -> Result<TransferReport, String> {
    let start = Instant::now();

    // Load source playlist metadata + tracks
    let source_svc = resolve_service(services, &req.source_service).await?;
    let (playlist_name, source_tracks) = {
        let svc = source_svc.lock().await;
        let playlist = svc.get_playlist(&req.source_playlist_id).await?;
        let tracks = svc.get_playlist_tracks(&req.source_playlist_id).await?;
        (playlist.name, tracks)
    };

    let total_tracks = source_tracks.len();
    let target_name = req
        .target_playlist_name
        .clone()
        .unwrap_or_else(|| playlist_name.clone());

    info!(
        source = %req.source_service,
        target = %req.target_service,
        playlist = %playlist_name,
        tracks = total_tracks,
        "playlist_transfer_start"
    );

    if total_tracks == 0 {
        return Err("source playlist has no tracks".into());
    }

    // Resolve target service
    let target_svc = resolve_service(services, &req.target_service).await?;

    // Match tracks on target
    let mut matched_ids: Vec<String> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();

    for track in &source_tracks {
        let found = search_track_on_target(&target_svc, &track.title, &track.artist).await;

        match found {
            Some((tid, _, _)) => {
                matched_ids.push(tid);
            }
            None => {
                let label = format!("{} - {}", track.artist, track.title);
                not_found.push(label);
                warn!(
                    title = %track.title,
                    artist = %track.artist,
                    "playlist_transfer_track_not_found"
                );
            }
        }
    }

    let matched = matched_ids.len();

    // Create target playlist
    let target_playlist_id = {
        let svc = target_svc.lock().await;
        svc.create_playlist(&target_name, None).await?
    };

    // Add matched tracks to target playlist
    if !matched_ids.is_empty() {
        let svc = target_svc.lock().await;
        let added = svc
            .add_tracks_to_playlist(&target_playlist_id, &matched_ids)
            .await?;
        info!(
            added = added,
            playlist_id = %target_playlist_id,
            "playlist_transfer_tracks_added"
        );
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    info!(
        matched = matched,
        not_found = not_found.len(),
        duration_ms = duration_ms,
        target_playlist_id = %target_playlist_id,
        "playlist_transfer_complete"
    );

    Ok(TransferReport {
        total_tracks,
        matched,
        not_found,
        target_playlist_id,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_request_deserialize() {
        let json = serde_json::json!({
            "source_service": "tidal",
            "source_playlist_id": "abc123",
            "target_service": "qobuz"
        });
        let req: TransferRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.source_service, "tidal");
        assert_eq!(req.target_service, "qobuz");
        assert!(req.target_playlist_name.is_none());
    }

    #[test]
    fn transfer_report_serialize() {
        let report = TransferReport {
            total_tracks: 10,
            matched: 8,
            not_found: vec!["Unknown - Song".into()],
            target_playlist_id: "pl-456".into(),
            duration_ms: 1234,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["total_tracks"], 10);
        assert_eq!(json["matched"], 8);
        assert_eq!(json["not_found"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn preview_match_serialize() {
        let m = PreviewMatch {
            source_title: "Imagine".into(),
            source_artist: "John Lennon".into(),
            target_track_id: Some("t-1".into()),
            target_title: Some("Imagine".into()),
            target_artist: Some("John Lennon".into()),
            status: "matched".into(),
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["status"], "matched");
        assert_eq!(json["target_track_id"], "t-1");
    }
}
