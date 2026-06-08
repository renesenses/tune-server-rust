use serde::Deserialize;
use tracing::{debug, info};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// A community-approved album cover returned by mozaiklabs.fr.
#[derive(Debug, Clone, Deserialize)]
pub struct CommunityCover {
    pub mbid_release: String,
    pub album_title: String,
    #[serde(default)]
    pub artist_name: Option<String>,
    pub image_url: String,
    pub approved_at: String,
}

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
        debug!(mbid, status = %status, "artist_image_report_rejected");
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
        debug!(album_id, status = %status, "genre_correction_rejected");
        return Err(format!("genre correction failed: {status}"));
    }

    info!(album_id, genre, "genre_correction_submitted");
    Ok(())
}

/// Submit a community album cover to mozaiklabs.fr for approval.
pub async fn submit_cover(
    base_url: &str,
    mbid_release: &str,
    album_title: &str,
    artist_name: Option<&str>,
    instance_id: &str,
    image_data: &[u8],
) -> Result<(), String> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/covers");
    let client = crate::http::client::shared();

    let mut form = reqwest::multipart::Form::new()
        .text("mbid_release", mbid_release.to_string())
        .text("album_title", album_title.to_string())
        .text("instance_id", instance_id.to_string());

    if let Some(artist) = artist_name {
        form = form.text("artist_name", artist.to_string());
    }

    let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name(format!("{mbid_release}.jpg"))
        .mime_str("image/jpeg")
        .map_err(|e| format!("mime error: {e}"))?;
    form = form.part("image", image_part);

    let resp = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit cover failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(mbid_release, status = %status, "cover_submit_rejected");
        return Err(format!("cover submit failed: {status}"));
    }

    info!(mbid_release, "community_cover_submitted");
    Ok(())
}

/// Fetch approved community covers from mozaiklabs.fr.
/// Pass `since` for incremental sync (ISO 8601 timestamp).
pub async fn fetch_approved_covers(
    base_url: &str,
    since: Option<&str>,
) -> Result<Vec<CommunityCover>, String> {
    let base = base_url.trim_end_matches('/');
    let mut url = format!("{base}/api/v1/community/covers/approved");
    if let Some(s) = since {
        url.push_str(&format!("?since={}", urlencoding::encode(s)));
    }
    let client = crate::http::client::shared();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("fetch approved covers failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(status = %status, "fetch_approved_covers_failed");
        return Err(format!("fetch approved covers failed: {status}"));
    }

    #[derive(Deserialize)]
    struct Wrapper {
        covers: Vec<CommunityCover>,
    }

    let wrapper: Wrapper = resp
        .json()
        .await
        .map_err(|e| format!("parse approved covers: {e}"))?;

    info!(count = wrapper.covers.len(), "community_covers_fetched");
    Ok(wrapper.covers)
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_url_constant() {
        assert_eq!(super::DEFAULT_BASE_URL, "https://mozaiklabs.fr");
    }
}
