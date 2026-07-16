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

    match lofty::read_from_path(audio_path) {
        Ok(tagged) => {
            if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                if let Some(pic) = tag.pictures().first() {
                    let mime = match pic.mime_type() {
                        Some(lofty::picture::MimeType::Jpeg) => "image/jpeg",
                        Some(lofty::picture::MimeType::Png) => "image/png",
                        Some(lofty::picture::MimeType::Bmp) => "image/bmp",
                        _ => "image/jpeg",
                    };
                    return Some((pic.data().to_vec(), mime.to_string()));
                }
            }
        }
        Err(e) => {
            debug!(
                path = %audio_path.display(),
                error = %e,
                "artwork_lofty_read_failed"
            );
        }
    }

    // DSF files store their ID3v2 tag — including embedded APIC artwork — at
    // an offset that lofty does not read, so the path above finds no picture.
    // Fall back to reading the cover directly from the DSF metadata chunk.
    crate::metadata::extract_dsf_cover(audio_path)
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
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    // Prefer the 1200px rendition for a crisp full-screen display (Now Playing
    // on Retina). Fall back to 500px if the larger size isn't available for
    // this release.
    for size in ["front-1200", "front-500"] {
        let url = format!("https://coverartarchive.org/release/{mbid}/{size}");
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        if resp.status().is_success() {
            let Ok(bytes) = resp.bytes().await else {
                continue;
            };
            // Reject tiny responses (likely error pages)
            if bytes.len() < 1000 {
                continue;
            }
            return Some(bytes.to_vec());
        }
    }
    None
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
    let client = crate::http::client::builder()
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

/// Resolve an artist's MusicBrainz ID from its name (best match).
///
/// Libraries whose files carry no MusicBrainz tags leave artists without an
/// MBID, so the rich MBID-based image sources (Fanart.tv / TheAudioDB /
/// MusicBrainz) can never find them. Look the artist up by name and accept only
/// a high-confidence match (MB returns a 0-100 score) to avoid mis-binding two
/// artists that share a name.
pub async fn search_musicbrainz_artist(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Unknown Artist") {
        return None;
    }
    let query = format!("artist:\"{}\"", trimmed.replace('"', ""));
    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let first = data.get("artists")?.as_array()?.first()?;
    // Only accept a confident match; MB scores the query 0-100.
    let score = first.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
    if score < 90 {
        return None;
    }
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
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());
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
                    &[&id.as_str() as &dyn crate::db::backend::ToSqlValue, album_id],
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
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
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
/// direct image → MusicBrainz→Wikidata→Wikimedia → Discogs → Last.fm.
pub async fn fetch_artist_image(
    mbid: &str,
    artist_name: &str,
    discogs_token: Option<&str>,
) -> Option<Vec<u8>> {
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    // 1. Mozaiklabs community by MBID (fastest, no rate limit) — highest priority
    if !mbid.is_empty() {
        if let Some(bytes) = fetch_artist_image_mozaiklabs(&client, mbid).await {
            return Some(bytes);
        }
    }

    // 1b. Mozaiklabs community by NAME — keeps mozaiklabs the top priority even
    // for artists without an MBID (which never reach the by-MBID lookup above),
    // BEFORE falling back to any external source.
    if !artist_name.is_empty() {
        if let Some(bytes) = fetch_artist_image_mozaiklabs_by_name(&client, artist_name).await {
            return Some(bytes);
        }
    }

    // Sources 2–5 are keyed by MBID; skip them entirely for artists without one
    // (avoids pointless requests + their rate-limit sleeps during a force pass).
    if !mbid.is_empty() {
        // 2. Fanart.tv
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(bytes) = fetch_artist_image_fanart(&client, mbid).await {
            return Some(bytes);
        }

        // 3. TheAudioDB (free API, good coverage)
        if let Some(bytes) = fetch_artist_image_theaudiodb(&client, mbid).await {
            return Some(bytes);
        }

        // 4+5. MusicBrainz: try direct image relation, then Wikidata→Wikimedia
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Some(bytes) = fetch_artist_image_musicbrainz_full(&client, mbid).await {
            return Some(bytes);
        }
    }

    // 6. Discogs (if token configured, search by artist name)
    if !artist_name.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(bytes) = fetch_artist_image_discogs(&client, artist_name, discogs_token).await {
            return Some(bytes);
        }
    }

    // 7. Last.fm (artist.getinfo → image array, "extralarge" or "mega")
    if !artist_name.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(bytes) = fetch_artist_image_lastfm(&client, artist_name).await {
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

/// Fetch an artist image from mozaiklabs.fr by **name** (community metadata),
/// via `GET /api/v1/artists/search?q=<name>`. Used as a fallback for artists
/// without an MBID so mozaiklabs stays the priority source for them too.
///
/// Requires an exact (case-insensitive) name match on a result that actually
/// has a non-empty `image_url`, to avoid grabbing the wrong artist from the
/// substring (`ilike %q%`) search.
async fn fetch_artist_image_mozaiklabs_by_name(
    client: &reqwest::Client,
    artist_name: &str,
) -> Option<Vec<u8>> {
    let q = artist_name.trim();
    if q.len() < 2 {
        return None;
    }
    let url = format!(
        "https://mozaiklabs.fr/api/v1/artists/search?q={}",
        urlencoding::encode(q)
    );
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let results = data.get("data")?.as_array()?;
    let want = q.to_lowercase();
    let image_url = results
        .iter()
        .filter(|a| {
            a.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.trim().to_lowercase() == want)
        })
        .find_map(|a| {
            a.get("image_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })?;
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
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
    token: Option<&str>,
) -> Option<Vec<u8>> {
    // Prefer the token passed by the caller (resolved from DB settings — where
    // the UI stores it), falling back to the environment. Previously this read
    // env only, so a Discogs token configured in the app never applied and no
    // artist images were fetched (Progman).
    let token = token
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TUNE_DISCOGS_TOKEN").ok())
        .or_else(|| std::env::var("DISCOGS_TOKEN").ok())?;
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

/// Fetch artist image from Last.fm using the `artist.getinfo` endpoint.
///
/// The response contains an `image` array with sizes: small, medium, large,
/// extralarge, mega. We prefer "mega" first, then "extralarge".
async fn fetch_artist_image_lastfm(client: &reqwest::Client, artist_name: &str) -> Option<Vec<u8>> {
    let api_key = std::env::var("TUNE_LASTFM_API_KEY")
        .or_else(|_| std::env::var("LASTFM_API_KEY"))
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .ok()?;
    if api_key.is_empty() {
        return None;
    }
    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "artist.getinfo"),
            ("artist", artist_name),
            ("api_key", &api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let images = data.pointer("/artist/image").and_then(|v| v.as_array())?;

    // Pick best available size: mega > extralarge > large
    let image_url = ["mega", "extralarge", "large"].iter().find_map(|&size| {
        images.iter().find_map(|img| {
            let s = img.get("size").and_then(|v| v.as_str())?;
            if s == size {
                let url = img.get("#text").and_then(|v| v.as_str())?;
                if !url.is_empty()
                    && !url.contains("/noimage/")
                    && !url.contains("2a96cbd8b46e442fc41c2b86b821562f")
                {
                    Some(url.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
    })?;

    download_image(client, &image_url).await
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
/// Whether an artist's recorded artwork actually exists (so enrichment can
/// re-fetch when the DB claims an image but the cache file is gone).
/// A remote `http(s)` `image_path` is served by redirect, so treat it as
/// present. Local paths are cache hashes → probe both `.jpg` and `.png`.
/// Whether the artwork referenced by `image_path` exists **as a local cache
/// file**. A remote `http(s)` URL counts as NOT cached: streaming services
/// (Tidal, Deezer, Amazon) store the artist picture as a remote URL, which
/// leaves no local file, is served only as a redirect that many renderers/
/// clients can't load, and blocks enrichment from ever caching a real image
/// (Fabien: full scan + Tidal premium, artwork_cache empty, no artist images).
/// Returning false for URLs makes enrichment localize them into the cache.
/// Also lets callers detect a stale DB `image_path` whose cache file is gone
/// (moved/wiped `artwork_cache`).
pub fn cached_artwork_exists(cache_dir: &std::path::Path, image_path: &str) -> bool {
    if image_path.starts_with("http") {
        return false;
    }
    cache_dir.join(format!("{image_path}.jpg")).exists()
        || cache_dir.join(format!("{image_path}.png")).exists()
}

/// Phase 1: Check community-approved images from mozaiklabs.fr first.
/// Phase 2: For remaining artists, fetch from mozaiklabs API / Fanart.tv / MusicBrainz,
/// then submit discovered images back to the community (fire-and-forget).
///
/// Respects rate limit: ~1 request/second.
pub async fn batch_enrich_artist_artwork(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    batch_enrich_artist_artwork_inner(db, cache_dir, false).await
}

/// Force variant: re-fetch artwork for EVERY artist with an MBID, ignoring the
/// "already has an image" guard. Fixes libraries where `image_path` is set to
/// stale/broken entries that never render (Fabien: full scan + premium, still
/// no artist images — the normal pass skips because the DB claims images exist).
pub async fn batch_refetch_artist_artwork(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    batch_enrich_artist_artwork_inner(db, cache_dir, true).await
}

async fn batch_enrich_artist_artwork_inner(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
    force: bool,
) {
    let artist_repo = crate::db::artist_repo::ArtistRepo::with_backend(db.clone());

    // --- Phase 1: Bulk-apply community-approved artist images ---
    let mut community_applied = 0u32;
    if let Ok(approved) =
        crate::cloud::community::fetch_approved_artist_images("https://mozaiklabs.fr", None).await
    {
        for img in &approved {
            // Check if this artist is in our DB and still needs an image.
            // Gate on the cache file actually existing, not just the DB column:
            // a scan can set image_path while the cache write failed, leaving a
            // grey square that would otherwise be skipped forever (Sandro).
            if let Ok(Some(artist)) = artist_repo.get_by_musicbrainz_id(&img.mbid) {
                // In force mode, re-apply even if the DB claims a cached image
                // (the point is to overwrite stale/broken entries).
                if !force
                    && artist
                        .image_path
                        .as_deref()
                        .is_some_and(|ip| cached_artwork_exists(&cache_dir, ip))
                {
                    continue;
                }
                let artist_id = match artist.id {
                    Some(id) => id,
                    None => continue,
                };
                let client = crate::http::client::builder()
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

    // --- Phase 2: Fetch from external sources ---
    // Force mode re-fetches EVERY artist (overwriting stale entries), including
    // those without an MBID — mozaiklabs-by-name + other by-name sources can
    // still find them. Normal mode only targets artists without an image.
    let mut artists = match if force {
        artist_repo.list_all_id_name_mbid()
    } else {
        artist_repo.list_without_image()
    } {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artist_artwork_list_failed");
            return;
        }
    };
    if force {
        info!(count = artists.len(), "batch_artist_artwork_force_refetch");
    }

    // Re-queue artists whose image_path is set in the DB but whose cache file is
    // actually missing. list_without_image only checks the column, so a scan
    // that set image_path while the cache write failed (or a cache that was
    // later cleared/moved) leaves a grey square that would be skipped forever
    // (Fabien: "j'ai pas les images d'artistes" despite a full scan + premium).
    // This extends the Phase-1 cache-existence guard (Sandro) to Phase 2.
    // Skipped in force mode, which already includes every MBID artist.
    if !force {
        match artist_repo.list_with_image_and_mbid() {
            Ok(with_image) => {
                let before = artists.len();
                for (id, name, mbid, image_path) in with_image {
                    if !cached_artwork_exists(&cache_dir, &image_path) {
                        artists.push((id, name, mbid));
                    }
                }
                let requeued = artists.len() - before;
                if requeued > 0 {
                    info!(requeued, "batch_artist_artwork_missing_cache_requeued");
                }
            }
            Err(e) => warn!(error = %e, "batch_artist_artwork_with_image_list_failed"),
        }
    }

    if artists.is_empty() {
        info!("batch_artist_artwork_skip_all_have_images");
        // Store result even when nothing to fetch
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
        settings
            .set(
                "artist_artwork_enrich_result",
                &serde_json::json!({
                    "status": "done",
                    "phase": "done",
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
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    // Discogs token as configured in the app UI (stored in settings), so the
    // by-name Discogs image lookup actually works (Progman: no artist images).
    let discogs_token = settings
        .get("discogs_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("TUNE_DISCOGS_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::env::var("DISCOGS_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
        });

    let mut enriched = 0u32;
    let mut failed = 0u32;
    let total_images = artists.len();

    for (i, (artist_id, name, mbid)) in artists.iter().enumerate() {
        // Rate limit: short delay between community lookups (no rate limit),
        // longer delay only when hitting external APIs (MusicBrainz etc.)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Resolve a MusicBrainz ID from the artist name when the files carried
        // no MB tag. Without it the rich image sources (Fanart/TheAudioDB/
        // MusicBrainz) and community matching can't find this artist — the whole
        // reason untagged libraries end up with almost no artist images. Persist
        // it so future runs and community lookups reuse it. MB asks for ~1 req/s,
        // so only pay that extra delay for artists we actually have to look up.
        let mut mbid = mbid.clone();
        if mbid.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            if let Some(found) = search_musicbrainz_artist(name).await {
                artist_repo.update_mbid(*artist_id, &found).ok();
                info!(artist_id, artist = %name, mbid = %found, "batch_artist_artwork_mbid_resolved");
                mbid = found;
            }
        }

        match fetch_artist_image(&mbid, name, discogs_token.as_deref()).await {
            Some(data) => {
                // Cache key: by MBID when known, else by NAME. Keying by
                // `artist-mbid-` with an EMPTY mbid made every artist without an
                // MBID collide on the same file (md5("artist-mbid-")), so they
                // overwrote each other's image (Keith Jarrett, Duke Ellington…
                // all sharing one photo). By-name matches Phase 3's convention.
                let key = if mbid.is_empty() {
                    format!("artist-name-{name}")
                } else {
                    format!("artist-mbid-{mbid}")
                };
                let hash = artwork_hash(&key);
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

                    // Fire-and-forget: submit to community for sharing.
                    // Only when we have an MBID — the community store is keyed by
                    // MBID, so submitting with an empty one is meaningless.
                    if !instance_id.is_empty() && !mbid.is_empty() {
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

        // Publish live progress for the UI (Fabien: enrichment looked frozen).
        if (i + 1) % 5 == 0 || i + 1 == total_images {
            settings
                .set(
                    "artist_artwork_enrich_result",
                    &serde_json::json!({
                        "status": "running",
                        "phase": "images",
                        "processed": i + 1,
                        "total": total_images,
                        "enriched": enriched,
                        "failed": failed,
                        "community_applied": community_applied,
                    })
                    .to_string(),
                )
                .ok();
        }
    }

    info!(
        total = artists.len(),
        enriched, failed, community_applied, "batch_artist_artwork_phase2_complete"
    );

    // --- Phase 3: Try Discogs + Last.fm by name for artists without MBID and without image ---
    let mut discogs_enriched = 0u32;
    let mut lastfm_enriched = 0u32;
    let discogs_available = discogs_token.is_some();
    let lastfm_available = std::env::var("TUNE_LASTFM_API_KEY")
        .or_else(|_| std::env::var("LASTFM_API_KEY"))
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    if discogs_available || lastfm_available {
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
                "batch_artist_artwork_phase3_started"
            );
            let client = crate::http::client::builder()
                .user_agent(MB_USER_AGENT)
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default();

            for (artist_id, name) in &no_mbid_artists {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                // Try Discogs first
                if discogs_available {
                    if let Some(data) =
                        fetch_artist_image_discogs(&client, name, discogs_token.as_deref()).await
                    {
                        let hash = artwork_hash(&format!("artist-name-{name}"));
                        std::fs::create_dir_all(&cache_dir).ok();
                        if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                            artist_repo.update_image(*artist_id, &hash, "discogs").ok();
                            discogs_enriched += 1;
                            info!(artist_id, artist = %name, "batch_artist_artwork_discogs_enriched");
                            continue;
                        }
                    }
                }

                // Fallback to Last.fm
                if lastfm_available {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if let Some(data) = fetch_artist_image_lastfm(&client, name).await {
                        let hash = artwork_hash(&format!("artist-name-{name}"));
                        std::fs::create_dir_all(&cache_dir).ok();
                        if save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                            artist_repo.update_image(*artist_id, &hash, "lastfm").ok();
                            lastfm_enriched += 1;
                            info!(artist_id, artist = %name, "batch_artist_artwork_lastfm_enriched");
                        }
                    }
                }
            }
            info!(
                discogs_enriched,
                lastfm_enriched,
                total = no_mbid_artists.len(),
                "batch_artist_artwork_phase3_complete"
            );
        }
    }

    let total_enriched = enriched + discogs_enriched + lastfm_enriched;
    info!(
        total_enriched,
        phase2_enriched = enriched,
        phase3_discogs = discogs_enriched,
        phase3_lastfm = lastfm_enriched,
        community_applied,
        "batch_artist_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    settings
        .set(
            "artist_artwork_enrich_result",
            &serde_json::json!({
                "status": "done",
                "phase": "done",
                "total": artists.len(),
                "enriched": total_enriched,
                "phase2_enriched": enriched,
                "phase3_discogs": discogs_enriched,
                "phase3_lastfm": lastfm_enriched,
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
            "artwork_extracted_but_save_failed_trying_folder"
        );
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

/// Re-extract embedded cover art for local albums that still have no
/// `cover_path`, reading directly from their track files (never the network).
///
/// The incremental scan only extracts covers from files it actually
/// re-processes; unchanged files are skipped. So an improvement to embedded-art
/// extraction (e.g. DSF ID3v2 covers stored at the DSF metadata offset that
/// lofty ignores — Thibaud) never reaches a library whose files are unchanged.
/// Running this at the end of a scan self-heals those albums: any local album
/// with a missing cover gets its embedded art re-extracted from the first track
/// that yields one. Returns the number of albums filled.
pub fn backfill_embedded_covers(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: &Path,
) -> usize {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::track_repo::TrackRepo;

    let album_repo = AlbumRepo::with_backend(db.clone());
    let track_repo = TrackRepo::with_backend(db.clone());
    let coverless = album_repo.list_without_cover().unwrap_or_default();

    let mut filled = 0usize;
    for (album_id, _title, _artist, _mbid) in &coverless {
        let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();
        for track in &tracks {
            let Some(ref file_path) = track.file_path else {
                continue;
            };
            if let Some(hash) = get_or_extract(Path::new(file_path), cache_dir) {
                if album_repo.force_update_cover_path(*album_id, &hash).is_ok() {
                    filled += 1;
                }
                break;
            }
        }
    }
    if filled > 0 {
        info!(filled, "backfill_embedded_covers_done");
    }
    filled
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
    fn backfill_fills_missing_covers_then_is_idempotent() {
        use crate::db::album_repo::AlbumRepo;
        use crate::db::artist_repo::ArtistRepo;
        use crate::db::backend::DbBackend;
        use crate::db::models::{Artist, Track};
        use crate::db::sqlite::SqliteDb;
        use crate::db::track_repo::TrackRepo;
        use std::sync::Arc;

        // Isolated temp dir: a track whose folder holds a cover.jpg. This
        // exercises the backfill wiring end to end (list_without_cover →
        // get_or_extract → force_update_cover_path); DSF ID3v2 extraction
        // itself is covered by the metadata parser path.
        let base = std::env::temp_dir().join(format!("tune_backfill_{}", std::process::id()));
        let music = base.join("album");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("cover.jpg"), b"\xff\xd8\xff\xe0dummyjpegdata").unwrap();
        let track_path = music.join("01.flac");
        std::fs::write(&track_path, b"not really flac").unwrap();
        let cache_dir = base.join("cache");

        let sqlite = SqliteDb::open_in_memory().unwrap();
        sqlite.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(sqlite);

        let artist_repo = ArtistRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend.clone());
        let track_repo = TrackRepo::with_backend(backend.clone());

        let aid = artist_repo
            .create(&Artist::new("Art Lande".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("While She Sleeps", aid, Some(1990))
            .unwrap()
            .id
            .unwrap();
        let mut track = Track::new("Snow Dance".into());
        track.artist_id = Some(aid);
        track.album_id = Some(alid);
        track.file_path = Some(track_path.to_string_lossy().into_owned());
        track_repo.create(&track).unwrap();

        // Album starts with no cover.
        assert!(
            album_repo
                .get(alid)
                .unwrap()
                .unwrap()
                .cover_path
                .as_deref()
                .unwrap_or("")
                .is_empty()
        );

        let filled = backfill_embedded_covers(&backend, &cache_dir);
        assert_eq!(filled, 1, "backfill should fill exactly one album");
        let cover = album_repo.get(alid).unwrap().unwrap().cover_path;
        assert!(
            cover.as_deref().is_some_and(|c| !c.is_empty()),
            "album cover_path should be set after backfill"
        );

        // Second run is a no-op: the album now has a cover.
        let filled_again = backfill_embedded_covers(&backend, &cache_dir);
        assert_eq!(
            filled_again, 0,
            "backfill must not re-process covered albums"
        );

        std::fs::remove_dir_all(&base).ok();
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
