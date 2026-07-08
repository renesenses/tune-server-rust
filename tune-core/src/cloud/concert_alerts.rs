use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::db::backend::DbBackend;

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

/// Push the library's artists (those with a MusicBrainz ID) as concert
/// subscriptions to the mozaiklabs cloud. Returns the number of artists
/// subscribed.
pub async fn sync_artist_subscriptions(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    // Get all artists with MusicBrainz IDs from local library
    let rows = backend
        .query_many(
            "SELECT DISTINCT musicbrainz_id, name FROM artists \
             WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != '' \
             LIMIT 200",
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("concert_alerts_no_artists");
        return Ok(0);
    }

    let artists: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "musicbrainz_artist_id": r.get(0).and_then(|v| v.as_string()),
                "artist_name": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect();

    let body = serde_json::json!({
        "instance_id": instance_id,
        "artists": artists,
    });

    let resp = http_client
        .post(format!("{CONCERTS_API}/subscribe"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("concert subscribe: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concert subscribe: HTTP {}", resp.status()));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let count = result["subscribed"].as_i64().unwrap_or(0) as usize;
    info!(count, "concert_subscriptions_synced");
    Ok(count)
}

/// Fetch upcoming concerts for artists that this instance has subscribed to.
pub async fn get_upcoming_concerts(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http_client
        .get(format!("{CONCERTS_API}/upcoming"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("concerts: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concerts: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let concerts = data["concerts"].as_array().cloned().unwrap_or_default();
    info!(count = concerts.len(), "upcoming_concerts_fetched");
    Ok(concerts)
}

/// Spawn a periodic background task that syncs artist subscriptions every
/// 24 hours and is gated behind the `community_sync_enabled` setting
/// (piggy-backs on the same toggle as community metadata sync).
pub fn spawn(backend: Arc<dyn DbBackend>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concert_alerts_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 2 minutes after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

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
                    if let Err(e) = sync_artist_subscriptions(&backend, &client, &instance_id).await
                    {
                        warn!(error = %e, "concert_subscriptions_sync_failed");
                    }
                } else {
                    debug!("concert_alerts_skipped_no_instance_id");
                }
            }

            // Every 24 hours
            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });
}
