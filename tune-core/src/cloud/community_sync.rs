use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};

const COMMUNITY_API: &str = "https://mozaiklabs.fr/api/v1/community/library";

/// Push enriched tracks (those with a MusicBrainz recording ID) to
/// mozaiklabs.fr so other Tune instances can benefit from the metadata.
/// Returns the number of tracks stored server-side.
pub async fn sync_enriched_tracks(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    // Query tracks with musicbrainz_recording_id set
    let rows = backend
        .query_many(
            "SELECT t.musicbrainz_recording_id, t.title, ar.name, al.title, t.genre, t.year, \
             t.composer, t.label, t.isrc, t.format, t.sample_rate, t.bit_depth \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE t.musicbrainz_recording_id IS NOT NULL \
               AND t.musicbrainz_recording_id != '' \
             LIMIT 100",
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("community_sync_no_tracks");
        return Ok(0);
    }

    let tracks: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "musicbrainz_recording_id": r.get(0).and_then(|v| v.as_string()),
                "title": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "album_title": r.get(3).and_then(|v| v.as_string()),
                "genre": r.get(4).and_then(|v| v.as_string()),
                "year": r.get(5).and_then(|v| v.as_i64()),
                "composer": r.get(6).and_then(|v| v.as_string()),
                "label": r.get(7).and_then(|v| v.as_string()),
                "isrc": r.get(8).and_then(|v| v.as_string()),
                "format": r.get(9).and_then(|v| v.as_string()),
                "sample_rate": r.get(10).and_then(|v| v.as_i64()),
                "bit_depth": r.get(11).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let body = serde_json::json!({
        "instance_id": instance_id,
        "tracks": tracks,
    });

    let resp = http_client
        .post(format!("{COMMUNITY_API}/tracks"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community sync: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("community sync: HTTP {}", resp.status()));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let stored = result["stored"].as_i64().unwrap_or(0) as usize;
    info!(stored, "community_tracks_synced");
    Ok(stored)
}

/// Pull enriched metadata from the community cloud and apply it to
/// local tracks that are missing genre/year/etc. Only fills in NULL
/// fields — never overwrites existing local metadata.
pub async fn pull_community_enrichments(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
) -> Result<usize, String> {
    let resp = http_client
        .get(format!("{COMMUNITY_API}/enriched"))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community pull: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("community pull: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let total = data["tracks"].as_array().map(|a| a.len()).unwrap_or(0);

    let mut applied = 0usize;
    if let Some(arr) = data["tracks"].as_array() {
        for t in arr {
            let mb_id = match t["musicbrainz_recording_id"].as_str() {
                Some(id) => id.to_string(),
                None => continue,
            };
            let genre = t["genre"].as_str().map(|s| s.to_string());
            let year = t["year"].as_i64().map(|v| v as i32);
            let composer = t["composer"].as_str().map(|s| s.to_string());
            let label = t["label"].as_str().map(|s| s.to_string());
            let isrc = t["isrc"].as_str().map(|s| s.to_string());

            let result = backend.execute(
                "UPDATE tracks SET \
                 genre = COALESCE(genre, ?), \
                 year = COALESCE(year, ?), \
                 composer = COALESCE(composer, ?), \
                 label = COALESCE(label, ?), \
                 isrc = COALESCE(isrc, ?) \
                 WHERE musicbrainz_recording_id = ? AND (genre IS NULL OR year IS NULL)",
                &[
                    &genre as &dyn ToSqlValue,
                    &year as &dyn ToSqlValue,
                    &composer as &dyn ToSqlValue,
                    &label as &dyn ToSqlValue,
                    &isrc as &dyn ToSqlValue,
                    &mb_id as &dyn ToSqlValue,
                ],
            );
            if result.is_ok() {
                applied += 1;
            }
        }
    }

    info!(pulled = total, applied, "community_enrichments_pulled");
    Ok(applied)
}

/// Spawn the periodic community sync task. Runs every 30 minutes,
/// gated behind the `community_sync_enabled` setting.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "community_sync_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 90s after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;

        loop {
            let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            let enabled = settings
                .get("community_sync_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);

            if enabled {
                let instance_id = settings
                    .get("instance_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if !instance_id.is_empty() {
                    if let Err(e) = sync_enriched_tracks(&backend, &client, &instance_id).await {
                        warn!(error = %e, "community_sync_push_failed");
                    }
                    if let Err(e) = pull_community_enrichments(&backend, &client).await {
                        warn!(error = %e, "community_sync_pull_failed");
                    }
                } else {
                    debug!("community_sync_skipped_no_instance_id");
                }
            }

            // Every 30 minutes
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    });
}
