use serde::{Deserialize, Serialize};
use tracing::info;

const RECO_API: &str = "https://mozaiklabs.fr/api/v1/premium/recommendations";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub recommended_album: String,
    pub recommended_artist: String,
    pub reason: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub cover_url: Option<String>,
    pub confidence: f64,
}

/// Generate recommendations based on library profile
pub async fn generate_recommendations(
    backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<Recommendation>, String> {
    // 1. Build library profile: top artists, genres, decades
    let top_artists = backend
        .query_many(
            "SELECT a.name, a.musicbrainz_id, COUNT(t.id) as track_count \
         FROM artists a JOIN tracks t ON t.artist_id = a.id \
         WHERE a.musicbrainz_id IS NOT NULL \
         GROUP BY a.id ORDER BY track_count DESC LIMIT 20",
            &[],
        )
        .map_err(|e| format!("query artists: {e}"))?;

    let _top_genres = backend.query_many(
        "SELECT genre, COUNT(*) as cnt FROM tracks WHERE genre IS NOT NULL AND genre != '' GROUP BY genre ORDER BY cnt DESC LIMIT 10",
        &[],
    ).map_err(|e| format!("query genres: {e}"))?;

    if top_artists.is_empty() {
        return Ok(vec![]);
    }

    // 2. For each top artist, find similar artists via MusicBrainz tags
    let mut recommendations = Vec::new();
    let mb_client = reqwest::Client::builder()
        .user_agent("TuneServer/1.0 (contact@mozaiklabs.fr)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| http_client.clone());

    let artist_names: Vec<String> = top_artists
        .iter()
        .filter_map(|r| r.get(0).and_then(|v| v.as_string()))
        .collect();

    // Query MusicBrainz for similar artists (via tags)
    for row in top_artists.iter().take(5) {
        let mb_id = match row.get(1).and_then(|v| v.as_string()) {
            Some(id) => id,
            None => continue,
        };
        let artist_name = row.get(0).and_then(|v| v.as_string()).unwrap_or_default();

        let resp = mb_client
            .get(format!("https://musicbrainz.org/ws/2/artist/{mb_id}"))
            .query(&[("inc", "tags"), ("fmt", "json")])
            .send()
            .await;

        if let Ok(r) = resp {
            if r.status().is_success() {
                if let Ok(data) = r.json::<serde_json::Value>().await {
                    // Get the artist's top tags
                    let tags: Vec<String> = data["tags"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| t["name"].as_str().map(String::from))
                                .take(3)
                                .collect()
                        })
                        .unwrap_or_default();

                    if !tags.is_empty() {
                        // Search for other artists with similar tags
                        let tag_query = tags
                            .iter()
                            .map(|t| format!("tag:{t}"))
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

                        let search_resp = mb_client
                            .get("https://musicbrainz.org/ws/2/artist")
                            .query(&[
                                ("query", tag_query.as_str()),
                                ("fmt", "json"),
                                ("limit", "5"),
                            ])
                            .send()
                            .await;

                        if let Ok(sr) = search_resp {
                            if sr.status().is_success() {
                                if let Ok(search_data) = sr.json::<serde_json::Value>().await {
                                    if let Some(artists) = search_data["artists"].as_array() {
                                        for similar in artists {
                                            let sim_name =
                                                similar["name"].as_str().unwrap_or("").to_string();
                                            // Skip if already in library
                                            if artist_names.iter().any(|n| {
                                                n.to_lowercase() == sim_name.to_lowercase()
                                            }) {
                                                continue;
                                            }
                                            let score = similar["score"].as_i64().unwrap_or(0)
                                                as f64
                                                / 100.0;
                                            recommendations.push(Recommendation {
                                                recommended_album: format!("Discover {sim_name}"),
                                                recommended_artist: sim_name,
                                                reason: Some(format!(
                                                    "Parce que vous écoutez {artist_name}"
                                                )),
                                                musicbrainz_release_group_id: None,
                                                cover_url: None,
                                                confidence: score,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    // Deduplicate by artist name
    recommendations.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    recommendations.dedup_by(|a, b| {
        a.recommended_artist.to_lowercase() == b.recommended_artist.to_lowercase()
    });
    recommendations.truncate(20);

    // Push to cloud for caching
    if !recommendations.is_empty() {
        let body = serde_json::json!({
            "instance_id": instance_id,
            "recommendations": recommendations,
        });
        let _ = http_client
            .post(RECO_API)
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;
    }

    info!(count = recommendations.len(), "recommendations_generated");
    Ok(recommendations)
}

/// Fetch cached recommendations from cloud
pub async fn get_recommendations(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http_client
        .get(RECO_API)
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("reco: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("reco: HTTP {}", resp.status()));
    }
    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    Ok(data["recommendations"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}
