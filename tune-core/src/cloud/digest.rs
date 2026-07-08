use tracing::info;

const DIGEST_API: &str = "https://mozaiklabs.fr/api/v1/premium";

/// Subscribe this instance to the weekly digest
pub async fn subscribe(
    http_client: &reqwest::Client,
    instance_id: &str,
    email: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "instance_id": instance_id,
        "email": email,
        "frequency": "weekly",
    });
    let resp = http_client
        .post(format!("{DIGEST_API}/digest/subscribe"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("digest subscribe: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("digest subscribe: HTTP {}", resp.status()));
    }
    info!("digest_subscribed");
    Ok(())
}

/// Fetch new releases for this instance's artists
pub async fn get_new_releases(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http_client
        .get(format!("{DIGEST_API}/digest/new-releases"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("digest: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("digest: HTTP {}", resp.status()));
    }
    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    Ok(data["releases"].as_array().cloned().unwrap_or_default())
}

/// Scan MusicBrainz for recent releases by library artists and push to cloud
pub async fn scan_new_releases(
    backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    http_client: &reqwest::Client,
) -> Result<usize, String> {
    // Get artists with MB IDs
    let rows = backend.query_many(
        "SELECT DISTINCT musicbrainz_id, name FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != '' LIMIT 50",
        &[],
    ).map_err(|e| format!("query: {e}"))?;

    let mut releases = Vec::new();
    let mb_client = crate::http::client::builder()
        .user_agent("TuneServer/1.0 (contact@mozaiklabs.fr)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| http_client.clone());

    for row in &rows {
        let mb_id = match row.get(0).and_then(|v| v.as_string()) {
            Some(id) => id,
            None => continue,
        };
        let artist_name = row.get(1).and_then(|v| v.as_string()).unwrap_or_default();

        // Query MusicBrainz for recent releases by this artist
        let resp = mb_client
            .get("https://musicbrainz.org/ws/2/release-group")
            .query(&[
                ("artist", mb_id.as_str()),
                ("type", "album"),
                ("limit", "5"),
                ("fmt", "json"),
            ])
            .send()
            .await;

        if let Ok(r) = resp {
            if r.status().is_success() {
                if let Ok(data) = r.json::<serde_json::Value>().await {
                    if let Some(groups) = data["release-groups"].as_array() {
                        for rg in groups {
                            let title = rg["title"].as_str().unwrap_or("").to_string();
                            let date = rg["first-release-date"].as_str().unwrap_or("").to_string();
                            let rg_id = rg["id"].as_str().map(String::from);
                            if date.len() >= 4 {
                                releases.push(serde_json::json!({
                                    "musicbrainz_artist_id": mb_id,
                                    "artist_name": artist_name,
                                    "album_title": title,
                                    "release_date": if date.len() >= 10 { &date[..10] } else { &date },
                                    "musicbrainz_release_id": rg_id,
                                }));
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    if releases.is_empty() {
        return Ok(0);
    }

    // Push to cloud in batches of 100
    let mut total = 0;
    for chunk in releases.chunks(100) {
        let body = serde_json::json!({ "releases": chunk });
        let resp = http_client
            .post(format!("{DIGEST_API}/digest/releases"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("push releases: {e}"))?;

        let count = if resp.status().is_success() {
            resp.json::<serde_json::Value>()
                .await
                .map(|d| d["stored"].as_i64().unwrap_or(0) as usize)
                .unwrap_or(0)
        } else {
            0
        };
        total += count;
    }

    info!(total, "new_releases_pushed");
    Ok(total)
}
