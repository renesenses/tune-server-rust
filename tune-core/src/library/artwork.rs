use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

/// Candidate filenames for folder-level cover art.
///
/// On case-insensitive filesystems (NTFS, APFS) duplicates are harmless.
/// On case-sensitive mounts (some NAS/SMB) we need several variants.
const FOLDER_COVER_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "Cover.jpg",
    "Cover.jpeg",
    "Cover.png",
    "Folder.jpg",
    "Folder.jpeg",
    "Folder.png",
    "Front.jpg",
    "Front.jpeg",
    "Front.png",
    "COVER.JPG",
    "COVER.JPEG",
    "COVER.PNG",
    "FOLDER.JPG",
    "FOLDER.JPEG",
    "FOLDER.PNG",
    "FRONT.JPG",
    "FRONT.JPEG",
    "FRONT.PNG",
];

const MB_USER_AGENT: &str = "Tune/0.1.0 (https://mozaiklabs.fr)";

pub fn extract_cover_art(audio_path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;

    let tagged = match lofty::read_from_path(audio_path) {
        Ok(t) => t,
        Err(e) => {
            debug!(
                path = %audio_path.display(),
                error = %e,
                "artwork_lofty_read_failed"
            );
            return None;
        }
    };
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
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        warn!(
            dir = %cache_dir.display(),
            error = %e,
            "artwork_cache_dir_create_failed — check directory permissions"
        );
        return None;
    }
    let filename = format!("{hash}.{ext}");
    let path = cache_dir.join(&filename);
    if let Err(e) = std::fs::write(&path, data) {
        warn!(
            path = %path.display(),
            error = %e,
            size = data.len(),
            "artwork_cache_write_failed — check directory permissions"
        );
        return None;
    }
    Some(path)
}

/// Compute a deterministic hash for an artwork cache key.
///
/// On Windows, backslashes are normalized to forward slashes so that the
/// same audio file always produces the same hash regardless of how the
/// path was constructed (e.g. `C:\Music\a.flac` and `C:/Music/a.flac`
/// yield the same hash).
pub fn artwork_hash(file_path: &str) -> String {
    use md5::{Digest, Md5};
    let normalized = file_path.replace('\\', "/");
    let mut hasher = Md5::new();
    hasher.update(normalized.as_bytes());
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
pub async fn batch_enrich_artwork(db: crate::db::sqlite::SqliteDb, cache_dir: PathBuf) {
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
        enriched, searched, failed, "batch_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    let settings = crate::db::settings_repo::SettingsRepo::new(db);
    settings
        .set(
            "artwork_enrich_result",
            &serde_json::json!({
                "total": albums.len(),
                "enriched": enriched,
                "searched": searched,
                "failed": failed,
            })
            .to_string(),
        )
        .ok();
}

/// Fetch an artist image from multiple sources (best-effort cascade).
///
/// Order: mozaiklabs community → Fanart.tv → TheAudioDB → MusicBrainz
/// direct image → MusicBrainz→Wikidata→Wikimedia → Discogs.
pub async fn fetch_artist_image(mbid: &str, artist_name: &str) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    // 1. Mozaiklabs community (fastest, no rate limit)
    if let Some(bytes) = fetch_artist_image_mozaiklabs(&client, mbid).await {
        return Some(bytes);
    }

    // 2. Fanart.tv
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if let Some(bytes) = fetch_artist_image_fanart(&client, mbid).await {
        return Some(bytes);
    }

    // 3. TheAudioDB (free API, good coverage)
    if let Some(bytes) = fetch_artist_image_theaudiodb(&client, mbid).await {
        return Some(bytes);
    }

    // 4+5. MusicBrainz: try direct image relation, then Wikidata→Wikimedia
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    if let Some(bytes) = fetch_artist_image_musicbrainz_full(&client, mbid).await {
        return Some(bytes);
    }

    // 6. Discogs (if token configured, search by artist name)
    if !artist_name.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Some(bytes) = fetch_artist_image_discogs(&client, artist_name).await {
            return Some(bytes);
        }
    }

    None
}

/// Fetch an artist thumbnail from Fanart.tv using a MusicBrainz artist ID.
async fn fetch_artist_image_fanart(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let api_key = std::env::var("FANART_TV_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    let url = format!("http://webservice.fanart.tv/v3/music/{mbid}?api_key={api_key}");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let thumb_url = data
        .get("artistthumb")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|obj| obj.get("url"))
        .and_then(|v| v.as_str())?;
    download_image(client, thumb_url).await
}

async fn fetch_artist_image_mozaiklabs(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let resp = client
        .get(format!("https://mozaiklabs.fr/api/v1/artists/{mbid}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let image_url = data
        .pointer("/data/image_url")
        .or_else(|| data.get("image_url"))
        .and_then(|v| v.as_str())?;
    if image_url.is_empty() {
        return None;
    }
    let full_url = if image_url.starts_with('/') {
        format!("https://mozaiklabs.fr{image_url}")
    } else {
        image_url.to_string()
    };
    download_image(client, &full_url).await
}

/// Fetch artist image from TheAudioDB (free API key "2").
async fn fetch_artist_image_theaudiodb(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let url = format!("https://theaudiodb.com/api/v1/json/2/artist-mb.php?i={mbid}");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let artist = data["artists"].as_array()?.first()?;
    let thumb_url = artist["strArtistThumb"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| artist["strArtistFanart"].as_str().filter(|s| !s.is_empty()))
        .or_else(|| artist["strArtistCutout"].as_str().filter(|s| !s.is_empty()))?;
    download_image(client, thumb_url).await
}

/// Fetch artist image from MusicBrainz: tries direct Wikimedia image relation
/// first, then falls back to Wikidata → P18 image property.
async fn fetch_artist_image_musicbrainz_full(
    client: &reqwest::Client,
    mbid: &str,
) -> Option<Vec<u8>> {
    let url = format!("https://musicbrainz.org/ws/2/artist/{mbid}?inc=url-rels&fmt=json");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let relations = match data["relations"].as_array() {
        Some(r) => r,
        None => return None,
    };

    // Try direct Wikimedia Commons image relation
    if let Some(commons_page) = relations.iter().find_map(|r| {
        if r["type"].as_str() == Some("image") {
            r["url"]["resource"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    }) {
        if let Some(filename) = commons_page.rsplit("File:").next() {
            let direct_url = format!(
                "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}?width=500",
                filename.replace(' ', "_")
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Some(bytes) = download_image(client, &direct_url).await {
                return Some(bytes);
            }
        }
    }

    // Fallback: Wikidata relation → P18 image → Wikimedia Commons
    let wikidata_url = relations.iter().find_map(|r| {
        if r["type"].as_str() == Some("wikidata") {
            r["url"]["resource"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    })?;
    let qid = wikidata_url.rsplit('/').next()?;
    if !qid.starts_with('Q') {
        return None;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    fetch_image_from_wikidata(client, qid).await
}

/// Resolve a Wikidata entity QID to an image via the P18 property.
async fn fetch_image_from_wikidata(client: &reqwest::Client, qid: &str) -> Option<Vec<u8>> {
    let url = format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let image_filename = data
        .pointer(&format!("/entities/{qid}/claims/P18"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|claim| claim.pointer("/mainsnak/datavalue/value"))
        .and_then(|v| v.as_str())?;
    let direct_url = format!(
        "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}?width=500",
        image_filename.replace(' ', "_")
    );
    download_image(client, &direct_url).await
}

/// Fetch artist image from Discogs by searching the artist name.
async fn fetch_artist_image_discogs(
    client: &reqwest::Client,
    artist_name: &str,
) -> Option<Vec<u8>> {
    let token = std::env::var("TUNE_DISCOGS_TOKEN")
        .or_else(|_| std::env::var("DISCOGS_TOKEN"))
        .ok()?;
    if token.is_empty() {
        return None;
    }
    let resp = client
        .get("https://api.discogs.com/database/search")
        .query(&[("type", "artist"), ("per_page", "1"), ("q", artist_name)])
        .header("Authorization", format!("Discogs token={token}"))
        .header("User-Agent", "TuneServer/1.0")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let cover_url = data["results"]
        .as_array()?
        .first()?
        .get("cover_image")
        .and_then(|v| v.as_str())
        .filter(|s| !s.contains("spacer.gif"))?;
    download_image(client, cover_url).await
}

async fn download_image(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() < 1000 {
        return None;
    }
    Some(bytes.to_vec())
}

/// Run batch artist image enrichment for all artists with an MBID but no image.
///
/// Phase 1: Check community-approved images from mozaiklabs.fr first.
/// Phase 2: For remaining artists, fetch from mozaiklabs API / Fanart.tv / MusicBrainz,
/// then submit discovered images back to the community (fire-and-forget).
///
/// Respects rate limit: ~1 request/second.
pub async fn batch_enrich_artist_artwork(db: crate::db::sqlite::SqliteDb, cache_dir: PathBuf) {
    let artist_repo = crate::db::artist_repo::ArtistRepo::new(db.clone());

    // --- Phase 1: Bulk-apply community-approved artist images ---
    let mut community_applied = 0u32;
    if let Ok(approved) =
        crate::cloud::community::fetch_approved_artist_images("https://mozaiklabs.fr", None).await
    {
        for img in &approved {
            // Check if this artist is in our DB and still needs an image
            if let Ok(Some(artist)) = artist_repo.get_by_musicbrainz_id(&img.mbid) {
                if artist.image_path.is_some() {
                    continue;
                }
                let artist_id = match artist.id {
                    Some(id) => id,
                    None => continue,
                };
                let client = reqwest::Client::builder()
                    .user_agent(MB_USER_AGENT)
                    .timeout(std::time::Duration::from_secs(15))
                    .build();
                if let Ok(client) = client {
                    if let Some(data) = download_image(&client, &img.image_url).await {
                        let hash = artwork_hash(&format!("artist-mbid-{}", img.mbid));
                        std::fs::create_dir_all(&cache_dir).ok();
                        if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                            artist_repo.update_image(artist_id, &hash, "community").ok();
                            community_applied += 1;
                            info!(
                                artist_id,
                                artist = %img.artist_name,
                                hash = %hash,
                                "batch_artist_artwork_community_applied"
                            );
                        }
                    }
                }
            }
        }
        if community_applied > 0 {
            info!(
                community_applied,
                "batch_artist_artwork_community_phase_done"
            );
        }
    }

    // --- Phase 2: Fetch from external sources for remaining artists ---
    let artists = match artist_repo.list_without_image() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artist_artwork_list_failed");
            return;
        }
    };

    if artists.is_empty() {
        info!("batch_artist_artwork_skip_all_have_images");
        // Store result even when nothing to fetch
        let settings = crate::db::settings_repo::SettingsRepo::new(db);
        settings
            .set(
                "artist_artwork_enrich_result",
                &serde_json::json!({
                    "total": 0,
                    "enriched": 0,
                    "failed": 0,
                    "community_applied": community_applied,
                })
                .to_string(),
            )
            .ok();
        return;
    }

    info!(
        count = artists.len(),
        "batch_artist_artwork_enrichment_started"
    );

    // Get instance_id for community submissions
    let settings = crate::db::settings_repo::SettingsRepo::new(db.clone());
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    let mut enriched = 0u32;
    let mut failed = 0u32;

    for (artist_id, name, mbid) in &artists {
        // Rate limit: wait 1.1s between requests
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        match fetch_artist_image(mbid, name).await {
            Some(data) => {
                // Use artist-specific hash key (same as manual upload)
                let hash = artwork_hash(&format!("artist-mbid-{mbid}"));
                std::fs::create_dir_all(&cache_dir).ok();
                if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                    artist_repo.update_image(*artist_id, &hash, "auto").ok();
                    enriched += 1;
                    info!(
                        artist_id,
                        artist = %name,
                        hash = %hash,
                        size = data.len(),
                        "batch_artist_artwork_enriched"
                    );

                    // Fire-and-forget: submit to community for sharing
                    if !instance_id.is_empty() {
                        let mbid = mbid.clone();
                        let name = name.clone();
                        let instance_id = instance_id.clone();
                        let image_data = data.clone();
                        tokio::spawn(async move {
                            if let Err(e) = crate::cloud::community::submit_artist_image(
                                "https://mozaiklabs.fr",
                                &mbid,
                                &name,
                                &instance_id,
                                &image_data,
                            )
                            .await
                            {
                                debug!(mbid = %mbid, error = %e, "community_artist_image_submit_failed");
                            }
                        });
                    }
                } else {
                    failed += 1;
                    warn!(artist_id, artist = %name, "batch_artist_artwork_save_failed");
                }
            }
            None => {
                failed += 1;
                debug!(artist_id, artist = %name, mbid = %mbid, "batch_artist_artwork_not_found");
            }
        }
    }

    info!(
        total = artists.len(),
        enriched, failed, community_applied, "batch_artist_artwork_phase2_complete"
    );

    // --- Phase 3: Try Discogs by name for artists without MBID and without image ---
    let mut discogs_enriched = 0u32;
    let discogs_available = std::env::var("TUNE_DISCOGS_TOKEN")
        .or_else(|_| std::env::var("DISCOGS_TOKEN"))
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    if discogs_available {
        let no_mbid_artists = match artist_repo.list_without_image_no_mbid() {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "batch_artist_artwork_no_mbid_list_failed");
                Vec::new()
            }
        };

        if !no_mbid_artists.is_empty() {
            info!(
                count = no_mbid_artists.len(),
                "batch_artist_artwork_phase3_discogs_started"
            );
            let client = reqwest::Client::builder()
                .user_agent(MB_USER_AGENT)
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default();

            for (artist_id, name) in &no_mbid_artists {
                // Discogs rate limit: 60 req/min → 1s between requests
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

                if let Some(data) = fetch_artist_image_discogs(&client, name).await {
                    let hash = artwork_hash(&format!("artist-name-{name}"));
                    std::fs::create_dir_all(&cache_dir).ok();
                    if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                        artist_repo.update_image(*artist_id, &hash, "discogs").ok();
                        discogs_enriched += 1;
                        info!(artist_id, artist = %name, "batch_artist_artwork_discogs_enriched");
                    }
                }
            }
            info!(
                discogs_enriched,
                total = no_mbid_artists.len(),
                "batch_artist_artwork_phase3_complete"
            );
        }
    }

    let total_enriched = enriched + discogs_enriched;
    info!(
        total_enriched,
        phase2_enriched = enriched,
        phase3_discogs = discogs_enriched,
        community_applied,
        "batch_artist_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    settings
        .set(
            "artist_artwork_enrich_result",
            &serde_json::json!({
                "total": artists.len(),
                "enriched": total_enriched,
                "phase2_enriched": enriched,
                "phase3_discogs": discogs_enriched,
                "failed": failed,
                "community_applied": community_applied,
            })
            .to_string(),
        )
        .ok();
}

pub fn get_or_extract(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    let hash = artwork_hash(&audio_path.to_string_lossy());

    // Check if already cached
    let cached_jpg = cache_dir.join(format!("{hash}.jpg"));
    let cached_png = cache_dir.join(format!("{hash}.png"));
    if cached_jpg.exists() {
        return Some(hash);
    }
    if cached_png.exists() {
        return Some(hash);
    }

    // Try embedded cover art from the audio file tags
    if let Some((data, mime)) = extract_cover_art(audio_path) {
        let ext = if mime.contains("png") { "png" } else { "jpg" };
        if save_to_cache(&data, cache_dir, &hash, ext).is_some() {
            return Some(hash);
        }
        warn!(
            path = %audio_path.display(),
            cache_dir = %cache_dir.display(),
            "artwork_extracted_but_save_failed"
        );
        return None;
    }

    // Try folder-level cover art (cover.jpg, folder.jpg, front.jpg, etc.)
    if let Some(folder_cover) = find_folder_cover(audio_path) {
        match std::fs::read(&folder_cover) {
            Ok(data) => {
                let ext = folder_cover
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg");
                if save_to_cache(&data, cache_dir, &hash, ext).is_some() {
                    debug!(
                        folder_cover = %folder_cover.display(),
                        "artwork_from_folder_cover"
                    );
                    return Some(hash);
                }
                warn!(
                    path = %folder_cover.display(),
                    cache_dir = %cache_dir.display(),
                    "folder_cover_read_but_save_failed"
                );
            }
            Err(e) => {
                debug!(
                    path = %folder_cover.display(),
                    error = %e,
                    "folder_cover_read_failed"
                );
            }
        }
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
        let result = get_or_extract(Path::new("/tmp/nonexistent_audio_file.flac"), &cache_dir);
        assert!(result.is_none());
    }

    #[test]
    fn artwork_hash_normalizes_backslashes() {
        // Windows path with backslashes should produce the same hash
        // as the equivalent path with forward slashes
        let h_win = artwork_hash("C:\\Users\\Scordia\\Music\\album\\track.flac");
        let h_unix = artwork_hash("C:/Users/Scordia/Music/album/track.flac");
        assert_eq!(h_win, h_unix);
    }

    #[test]
    fn artwork_hash_forward_slashes_unchanged() {
        // Pure Unix paths should hash identically before and after normalization
        let h = artwork_hash("/music/artist/album/track.flac");
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
