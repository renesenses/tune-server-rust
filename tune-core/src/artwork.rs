use std::path::{Path, PathBuf};

use tracing::{info, warn, debug};

const FOLDER_COVER_NAMES: &[&str] = &[
    "cover.jpg", "cover.png", "folder.jpg", "folder.png",
    "front.jpg", "front.png", "album.jpg", "album.png",
    "Cover.jpg", "Cover.png", "Folder.jpg", "Folder.png",
    "Front.jpg", "Front.png",
];

const MB_USER_AGENT: &str = "Tune/0.1.0 (https://mozaiklabs.fr)";

pub fn extract_cover_art(audio_path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;

    let tagged = lofty::read_from_path(audio_path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;

    let mime = match pic.mime_type() {
        Some(lofty::picture::MimeType::Jpeg) => "image/jpeg",
        Some(lofty::picture::MimeType::Png) => "image/png",
        Some(lofty::picture::MimeType::Bmp) => "image/bmp",
        _ => "image/jpeg",
    };

    Some((pic.data().to_vec(), mime.to_string()))
}

pub fn find_folder_cover(audio_path: &Path) -> Option<PathBuf> {
    let dir = audio_path.parent()?;
    for name in FOLDER_COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub fn save_to_cache(data: &[u8], cache_dir: &Path, hash: &str, ext: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(cache_dir).ok()?;
    let filename = format!("{hash}.{ext}");
    let path = cache_dir.join(&filename);
    std::fs::write(&path, data).ok()?;
    Some(path)
}

pub fn artwork_hash(file_path: &str) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    hasher.update(file_path.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetch front cover art from the Cover Art Archive using a MusicBrainz release ID.
pub async fn fetch_cover_art(mbid: &str) -> Option<Vec<u8>> {
    let url = format!("https://coverartarchive.org/release/{mbid}/front-500");
    let client = reqwest::Client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if resp.status().is_success() {
        let bytes = resp.bytes().await.ok()?;
        // Reject tiny responses (likely error pages)
        if bytes.len() < 1000 {
            return None;
        }
        Some(bytes.to_vec())
    } else {
        None
    }
}

/// Search MusicBrainz for a release MBID by artist name and album title.
/// Returns the first matching release ID, or None.
pub async fn search_musicbrainz_release(artist: &str, title: &str) -> Option<String> {
    let query = format!(
        "release:\"{}\" AND artist:\"{}\"",
        title.replace('"', ""),
        artist.replace('"', "")
    );
    let url = format!(
        "https://musicbrainz.org/ws/2/release/?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );
    let client = reqwest::Client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let releases = data.get("releases")?.as_array()?;
    let first = releases.first()?;
    first.get("id")?.as_str().map(|s| s.to_string())
}

/// Run batch artwork enrichment for all albums missing cover art.
///
/// Iterates over albums without a `cover_path`, tries Cover Art Archive
/// (by existing MBID or by searching MusicBrainz), saves the image to the
/// artwork cache, and updates the album's `cover_path` in the database.
///
/// Respects MusicBrainz rate limit: max 1 request/second.
pub async fn batch_enrich_artwork(
    db: crate::db::sqlite::SqliteDb,
    cache_dir: PathBuf,
) {
    let album_repo = crate::db::album_repo::AlbumRepo::new(db.clone());
    let albums = match album_repo.list_without_cover() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artwork_list_failed");
            return;
        }
    };

    if albums.is_empty() {
        info!("batch_artwork_skip_all_have_covers");
        return;
    }

    info!(count = albums.len(), "batch_artwork_enrichment_started");

    let mut enriched = 0u32;
    let mut searched = 0u32;
    let mut failed = 0u32;

    for (album_id, title, artist_name, mbid) in &albums {
        let artist = artist_name.as_deref().unwrap_or("Unknown Artist");

        // Step 1: Determine MBID — use existing or search MusicBrainz
        let resolved_mbid = if let Some(id) = mbid {
            if !id.is_empty() {
                Some(id.clone())
            } else {
                None
            }
        } else {
            None
        };

        let mbid_to_use = if let Some(id) = resolved_mbid {
            Some(id)
        } else {
            // Search MusicBrainz for the release
            searched += 1;
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            let found = search_musicbrainz_release(artist, title).await;
            if let Some(ref id) = found {
                // Store the discovered MBID on the album for future use
                db.execute(
                    "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                    &[id as &dyn rusqlite::types::ToSql, album_id],
                ).ok();
                debug!(album_id, mbid = %id, album = %title, "batch_artwork_mbid_found");
            }
            found
        };

        let Some(ref mbid_val) = mbid_to_use else {
            debug!(album_id, album = %title, artist = %artist, "batch_artwork_no_mbid");
            failed += 1;
            continue;
        };

        // Step 2: Fetch cover from Cover Art Archive
        // Rate limit: wait 1.1s between CAA requests
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        match fetch_cover_art(mbid_val).await {
            Some(data) => {
                let hash = artwork_hash(mbid_val);
                std::fs::create_dir_all(&cache_dir).ok();
                if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                    album_repo.update_cover_path(*album_id, &hash).ok();
                    enriched += 1;
                    info!(
                        album_id,
                        album = %title,
                        artist = %artist,
                        hash = %hash,
                        size = data.len(),
                        "batch_artwork_enriched"
                    );
                } else {
                    failed += 1;
                    warn!(album_id, album = %title, "batch_artwork_save_failed");
                }
            }
            None => {
                failed += 1;
                debug!(album_id, album = %title, mbid = %mbid_val, "batch_artwork_caa_not_found");
            }
        }
    }

    info!(
        total = albums.len(),
        enriched,
        searched,
        failed,
        "batch_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    let settings = crate::db::settings_repo::SettingsRepo::new(db);
    settings.set(
        "artwork_enrich_result",
        &serde_json::json!({
            "total": albums.len(),
            "enriched": enriched,
            "searched": searched,
            "failed": failed,
        }).to_string(),
    ).ok();
}

pub fn get_or_extract(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    let hash = artwork_hash(&audio_path.to_string_lossy());

    let cached_jpg = cache_dir.join(format!("{hash}.jpg"));
    let cached_png = cache_dir.join(format!("{hash}.png"));
    if cached_jpg.exists() {
        return Some(hash);
    }
    if cached_png.exists() {
        return Some(hash);
    }

    if let Some((data, mime)) = extract_cover_art(audio_path) {
        let ext = if mime.contains("png") { "png" } else { "jpg" };
        save_to_cache(&data, cache_dir, &hash, ext);
        return Some(hash);
    }

    if let Some(folder_cover) = find_folder_cover(audio_path)
        && let Ok(data) = std::fs::read(&folder_cover) {
            let ext = folder_cover.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            save_to_cache(&data, cache_dir, &hash, ext);
            return Some(hash);
        }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_hash_deterministic() {
        let h1 = artwork_hash("/music/test.flac");
        let h2 = artwork_hash("/music/test.flac");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn nonexistent_file_returns_none() {
        assert!(extract_cover_art(Path::new("/tmp/nonexistent.flac")).is_none());
    }

    #[test]
    fn artwork_hash_different_for_different_paths() {
        let h1 = artwork_hash("/music/a.flac");
        let h2 = artwork_hash("/music/b.flac");
        assert_ne!(h1, h2);
    }

    #[test]
    fn artwork_hash_hex_chars() {
        let h = artwork_hash("/test");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn artwork_hash_empty_string() {
        let h = artwork_hash("");
        assert_eq!(h.len(), 32);
        // MD5 of empty string
        assert_eq!(h, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn artwork_hash_unicode_path() {
        let h = artwork_hash("/music/Rene/album.flac");
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn find_folder_cover_nonexistent_dir() {
        let result = find_folder_cover(Path::new("/tmp/nonexistent_dir_12345/track.flac"));
        assert!(result.is_none());
    }

    #[test]
    fn save_to_cache_and_read() {
        let dir = std::env::temp_dir().join("tune_test_artwork_cache");
        let _ = std::fs::remove_dir_all(&dir);

        let data = b"fake image data";
        let result = save_to_cache(data, &dir, "test_hash_123", "jpg");
        assert!(result.is_some());

        let path = result.unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_to_cache_creates_dir() {
        let dir = std::env::temp_dir().join("tune_test_artwork_new_dir");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());

        save_to_cache(b"test", &dir, "hash", "png");
        assert!(dir.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_or_extract_nonexistent() {
        let cache_dir = std::env::temp_dir().join("tune_test_extract_ne");
        let result = get_or_extract(
            Path::new("/tmp/nonexistent_audio_file.flac"),
            &cache_dir,
        );
        assert!(result.is_none());
    }
}
