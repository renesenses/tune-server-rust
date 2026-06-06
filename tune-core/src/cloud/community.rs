use tracing::{info, warn};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// Report a community-sourced artist image for a given MusicBrainz ID.
pub async fn report_artist_image(
    mbid: &str,
    image_url: &str,
    base_url: Option<&str>,
) -> Result<(), String> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
    let url = format!("{base}/api/v1/artists/{mbid}/image/report");
    let client = crate::http::client::shared();

    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "image_url": image_url }))
        .send()
        .await
        .map_err(|e| format!("report artist image failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        warn!(mbid, status = %status, "artist_image_report_rejected");
        return Err(format!("artist image report failed: {status}"));
    }

    info!(mbid, "artist_image_reported");
    Ok(())
}

/// Submit a genre correction for an album.
pub async fn submit_genre_correction(
    album_id: &str,
    genre: &str,
    base_url: Option<&str>,
) -> Result<(), String> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
    let url = format!("{base}/api/v1/community/genres");
    let client = crate::http::client::shared();

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "album_id": album_id,
            "genre": genre,
        }))
        .send()
        .await
        .map_err(|e| format!("genre correction submit failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        warn!(album_id, status = %status, "genre_correction_rejected");
        return Err(format!("genre correction failed: {status}"));
    }

    info!(album_id, genre, "genre_correction_submitted");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_url_constant() {
        assert_eq!(super::DEFAULT_BASE_URL, "https://mozaiklabs.fr");
    }
}
