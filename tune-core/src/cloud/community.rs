use serde::Deserialize;
use tracing::{debug, info};

use crate::cloud::refusal::CloudError;

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
) -> Result<(), CloudError> {
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
        return Err(CloudError::from_response(
            format!("artist image report failed: {status}"),
            resp,
        )
        .await);
    }

    info!(mbid, "artist_image_reported");
    Ok(())
}

/// A metadata report to submit to the community backend.
#[derive(Debug, Clone, Default)]
pub struct ReportSubmission<'a> {
    pub entity: &'a str,
    pub mbid: Option<&'a str>,
    pub field: Option<&'a str>,
    pub value: Option<&'a str>,
    pub reason: &'a str,
    pub comment: Option<&'a str>,
}

/// Submit a metadata report (wrong cover, wrong credit, bogus bio…).
/// Values reported by enough distinct instances get unpublished server-side.
pub async fn submit_report(
    base_url: &str,
    instance_id: &str,
    report: &ReportSubmission<'_>,
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/reports");
    let client = crate::http::client::shared();

    let mut body = serde_json::json!({
        "instance_id": instance_id,
        "entity": report.entity,
        "reason": report.reason,
    });
    for (key, val) in [
        ("mbid", report.mbid),
        ("field", report.field),
        ("value", report.value),
        ("comment", report.comment),
    ] {
        if let Some(v) = val {
            body[key] = serde_json::json!(v);
        }
    }

    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit report failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(entity = report.entity, status = %status, "metadata_report_rejected");
        return Err(
            CloudError::from_response(format!("metadata report failed: {status}"), resp).await,
        );
    }

    info!(
        entity = report.entity,
        reason = report.reason,
        "metadata_report_submitted"
    );
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
) -> Result<(), CloudError> {
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
        return Err(
            CloudError::from_response(format!("cover submit failed: {status}"), resp).await,
        );
    }

    info!(mbid_release, "community_cover_submitted");
    Ok(())
}

/// Fetch approved community covers from mozaiklabs.fr.
/// Pass `since` for incremental sync (ISO 8601 timestamp).
pub async fn fetch_approved_covers(
    base_url: &str,
    since: Option<&str>,
) -> Result<Vec<CommunityCover>, CloudError> {
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
        return Err(CloudError::from_response(
            format!("fetch approved covers failed: {status}"),
            resp,
        )
        .await);
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

/// A community-approved artist image returned by mozaiklabs.fr.
#[derive(Debug, Clone, Deserialize)]
pub struct CommunityArtistImage {
    pub mbid: String,
    pub artist_name: String,
    pub image_url: String,
    pub approved_at: String,
}

/// Submit a community artist image to mozaiklabs.fr for approval.
pub async fn submit_artist_image(
    base_url: &str,
    mbid: &str,
    artist_name: &str,
    instance_id: &str,
    image_data: &[u8],
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/artist-images");
    let client = crate::http::client::shared();

    let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name(format!("{mbid}.jpg"))
        .mime_str("image/jpeg")
        .map_err(|e| format!("mime error: {e}"))?;

    let form = reqwest::multipart::Form::new()
        .text("mbid", mbid.to_string())
        .text("artist_name", artist_name.to_string())
        .text("instance_id", instance_id.to_string())
        .part("image", image_part);

    let resp = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit artist image failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(mbid, status = %status, "artist_image_submit_rejected");
        return Err(CloudError::from_response(
            format!("artist image submit failed: {status}"),
            resp,
        )
        .await);
    }

    info!(mbid, artist_name, "community_artist_image_submitted");
    Ok(())
}

/// Fetch approved community artist images from mozaiklabs.fr.
pub async fn fetch_approved_artist_images(
    base_url: &str,
    since: Option<&str>,
) -> Result<Vec<CommunityArtistImage>, CloudError> {
    let base = base_url.trim_end_matches('/');
    let mut url = format!("{base}/api/v1/community/artist-images/approved");
    if let Some(s) = since {
        url.push_str(&format!("?since={}", urlencoding::encode(s)));
    }
    let client = crate::http::client::shared();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("fetch approved artist images failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(status = %status, "fetch_approved_artist_images_failed");
        return Err(CloudError::from_response(
            format!("fetch approved artist images failed: {status}"),
            resp,
        )
        .await);
    }

    #[derive(Deserialize)]
    struct Wrapper {
        images: Vec<CommunityArtistImage>,
    }

    let wrapper: Wrapper = resp
        .json()
        .await
        .map_err(|e| format!("parse approved artist images: {e}"))?;

    info!(
        count = wrapper.images.len(),
        "community_artist_images_fetched"
    );
    Ok(wrapper.images)
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_url_constant() {
        assert_eq!(super::DEFAULT_BASE_URL, "https://mozaiklabs.fr");
    }
}
