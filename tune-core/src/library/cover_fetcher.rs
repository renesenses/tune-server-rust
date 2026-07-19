use std::path::PathBuf;
use std::time::Duration;

use md5::{Digest, Md5};
use tracing::info;

const CAA_URL: &str = "https://coverartarchive.org/release";
const DISCOGS_API: &str = "https://api.discogs.com";
const DISCOGS_UA: &str = "TuneServer/1.0";

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub async fn fetch_cover_from_caa(musicbrainz_release_id: &str, cache_dir: &str) -> Option<String> {
    if musicbrainz_release_id.is_empty() {
        return None;
    }

    let client = crate::http::client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;

    // Prefer the 1200px rendition for a crisp full-screen display (Now Playing
    // on Retina); fall back to 500px if the larger size isn't available.
    let img_data = {
        let mut data = None;
        for size in ["front-1200", "front-500"] {
            let url = format!("{CAA_URL}/{musicbrainz_release_id}/{size}");
            let Ok(resp) = client.get(&url).send().await else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(bytes) = resp.bytes().await else {
                continue;
            };
            if bytes.len() >= 1000 {
                data = Some(bytes);
                break;
            }
        }
        data?
    };

    let h = md5_hex(musicbrainz_release_id);
    let cache_path = PathBuf::from(cache_dir).join(format!("caa_{h}.jpg"));
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&cache_path, &img_data).ok()?;

    info!(release_id = musicbrainz_release_id, "cover_fetched_caa");
    Some(cache_path.to_string_lossy().to_string())
}

pub async fn fetch_cover_from_discogs(
    album_title: &str,
    artist_name: &str,
    discogs_token: &str,
    cache_dir: &str,
) -> Option<String> {
    if discogs_token.is_empty() || album_title.is_empty() {
        return None;
    }

    let client = crate::http::client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;

    let query = if artist_name.is_empty() {
        album_title.to_string()
    } else {
        format!("{artist_name} {album_title}")
    };

    let resp = client
        .get(format!("{DISCOGS_API}/database/search"))
        .header("Authorization", format!("Discogs token={discogs_token}"))
        .header("User-Agent", DISCOGS_UA)
        .query(&[
            ("q", &query),
            ("type", &"release".to_string()),
            ("per_page", &"1".to_string()),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let results = data["results"].as_array()?;
    let first = results.first()?;

    let cover_url = first["cover_image"]
        .as_str()
        .or_else(|| first["thumb"].as_str())?;

    if cover_url.contains("spacer.gif") {
        return None;
    }

    let img_resp = client
        .get(cover_url)
        .header("User-Agent", DISCOGS_UA)
        .send()
        .await
        .ok()?;

    if !img_resp.status().is_success() {
        return None;
    }

    let img_data = img_resp.bytes().await.ok()?;
    if img_data.len() < 1000 {
        return None;
    }

    let h = md5_hex(cover_url);
    let cache_path = PathBuf::from(cache_dir).join(format!("discogs_{h}.jpg"));
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&cache_path, &img_data).ok()?;

    info!(album = album_title, "cover_fetched_discogs");
    Some(cache_path.to_string_lossy().to_string())
}

#[derive(Debug, Clone)]
pub struct CoverResult {
    pub source: String,
    pub local_path: String,
}

pub async fn search_covers(
    album_title: &str,
    artist_name: &str,
    musicbrainz_release_id: &str,
    discogs_token: &str,
    cache_dir: &str,
) -> Vec<CoverResult> {
    let mut results = Vec::new();

    if !musicbrainz_release_id.is_empty()
        && let Some(path) = fetch_cover_from_caa(musicbrainz_release_id, cache_dir).await
    {
        results.push(CoverResult {
            source: "coverartarchive".into(),
            local_path: path,
        });
    }

    if !discogs_token.is_empty()
        && let Some(path) =
            fetch_cover_from_discogs(album_title, artist_name, discogs_token, cache_dir).await
    {
        results.push(CoverResult {
            source: "discogs".into(),
            local_path: path,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_deterministic() {
        let a = md5_hex("test-release-id");
        let b = md5_hex("test-release-id");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn empty_release_id_returns_none() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(fetch_cover_from_caa("", "/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn empty_token_returns_none() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(fetch_cover_from_discogs("Album", "Artist", "", "/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn empty_album_returns_none() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(fetch_cover_from_discogs("", "Artist", "token", "/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn cache_path_format() {
        let h = md5_hex("abc-123");
        let path = PathBuf::from("/cache").join(format!("caa_{h}.jpg"));
        assert!(path.to_string_lossy().contains("caa_"));
        assert!(path.to_string_lossy().ends_with(".jpg"));
    }
}
