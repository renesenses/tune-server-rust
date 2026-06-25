use std::collections::HashMap;
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::Client;
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::*;
use crate::TuneError;

// YouTube Data API v3
const YT_API_BASE: &str = "https://www.googleapis.com/youtube/v3";

// YouTube Music internal API
const YTM_API_BASE: &str = "https://music.youtube.com/youtubei/v1";

// Stream URL cache TTL — YouTube CDN URLs expire after ~6 hours, cache for 5h
const STREAM_URL_TTL_SECS: u64 = 18_000;

// yt-dlp subprocess timeout
const YTDLP_TIMEOUT_SECS: u64 = 30;

/// YouTube Music context body for internal API calls.
/// This mimics the web client request format that YouTube Music expects.
fn ytm_context() -> serde_json::Value {
    json!({
        "client": {
            "clientName": "WEB_REMIX",
            "clientVersion": "1.20250620.01.00",
            "hl": "en",
            "gl": "US",
            "experimentIds": [],
            "experimentsToken": "",
            "browserName": "Chrome",
            "browserVersion": "137.0.0.0",
            "osName": "Windows",
            "osVersion": "10.0",
            "platform": "DESKTOP",
            "musicAppInfo": {
                "musicActivityMasterSwitch": "MUSIC_ACTIVITY_MASTER_SWITCH_INDETERMINATE",
                "musicLocationMasterSwitch": "MUSIC_LOCATION_MASTER_SWITCH_INDETERMINATE",
                "pwaInstallabilityStatus": "PWA_INSTALLABILITY_STATUS_UNKNOWN"
            }
        },
        "user": {
            "lockedSafetyMode": false
        }
    })
}

/// Android client context for the `/player` endpoint.
/// The Android client returns direct audio stream URLs without requiring
/// a browser-side JavaScript challenge (signature cipher), unlike WEB_REMIX.
fn ytm_android_context() -> serde_json::Value {
    json!({
        "client": {
            "clientName": "ANDROID_MUSIC",
            "clientVersion": "7.27.52",
            "androidSdkVersion": 30,
            "hl": "en",
            "gl": "US",
            "platform": "MOBILE",
            "osName": "Android",
            "osVersion": "11",
            "userAgent": "com.google.android.apps.youtube.music/7.27.52 (Linux; U; Android 11) gzip"
        },
        "user": {
            "lockedSafetyMode": false
        }
    })
}

/// Cached stream URL entry.
#[derive(Clone)]
struct CachedUrl {
    url: String,
    created: Instant,
}

/// URL cache with TTL-based expiration.
struct UrlCache {
    entries: HashMap<String, CachedUrl>,
    ttl: Duration,
}

impl UrlCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn get(&self, key: &str) -> Option<&CachedUrl> {
        self.entries.get(key).and_then(|entry| {
            if entry.created.elapsed() < self.ttl {
                Some(entry)
            } else {
                None
            }
        })
    }

    fn set(&mut self, key: String, entry: CachedUrl) {
        // Evict expired entries when cache grows large
        if self.entries.len() > 500 {
            let ttl = self.ttl;
            self.entries.retain(|_, e| e.created.elapsed() < ttl);
        }
        self.entries.insert(key, entry);
    }
}

/// Browse cache entry with timestamp.
struct BrowseCacheEntry {
    data: serde_json::Value,
    created: Instant,
}

/// YouTube Music streaming service — works without authentication.
///
/// Uses the YouTube Music internal API (`music.youtube.com/youtubei/v1`) for
/// search, browse, album/artist/playlist metadata. Uses `yt-dlp` as a
/// subprocess to extract audio stream URLs for DLNA playback.
///
/// Optional: YouTube Data API v3 with an API key (`TUNE_YOUTUBE_API_KEY`)
/// provides higher quota and more reliable video metadata.
pub struct YouTubeService {
    client: Client,
    /// Always true — YouTube works without auth for search/playback.
    authenticated: bool,
    url_cache: Mutex<UrlCache>,
    /// General-purpose browse/home cache (30 min TTL).
    browse_cache: Mutex<HashMap<String, BrowseCacheEntry>>,
    browse_cache_ttl: Duration,
    /// Optional YouTube Data API v3 key for higher quota.
    api_key: Option<String>,
    enabled_override: Option<bool>,
}

impl Default for YouTubeService {
    fn default() -> Self {
        Self::new()
    }
}

impl YouTubeService {
    pub fn new() -> Self {
        // Read API key from env if available
        let api_key = std::env::var("TUNE_YOUTUBE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(45))
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
                .build()
                .unwrap_or_else(|_| Client::new()),
            authenticated: true, // YouTube works without auth
            url_cache: Mutex::new(UrlCache::new(STREAM_URL_TTL_SECS)),
            browse_cache: Mutex::new(HashMap::new()),
            browse_cache_ttl: Duration::from_secs(1800), // 30 minutes
            api_key,
            enabled_override: None,
        }
    }

    // ------------------------------------------------------------------
    // YouTube Music internal API helpers
    // ------------------------------------------------------------------

    /// POST to the YouTube Music internal API.
    async fn ytm_post(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{YTM_API_BASE}/{endpoint}?prettyPrint=false");

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Origin", "https://music.youtube.com")
            .header("Referer", "https://music.youtube.com/")
            .header("X-Goog-Visitor-Id", "")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ytm api {endpoint}: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            info!(endpoint, status, body = %body_text.chars().take(200).collect::<String>(), "ytm_api_error");
            return Err(format!("ytm {endpoint}: {status}"));
        }

        resp.json()
            .await
            .map_err(|e| format!("ytm json parse: {e}"))
    }

    /// Search using the YouTube Music internal API.
    async fn ytm_search(
        &self,
        query: &str,
        filter: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let mut params = json!({
            "query": query,
            "context": ytm_context(),
        });

        // Add filter for specific result types
        if let Some(f) = filter {
            let filter_param = match f {
                "songs" => "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D",
                "albums" => "EgWKAQIYAWoKEAkQBRAKEAMQBA%3D%3D",
                "artists" => "EgWKAQIgAWoKEAkQBRAKEAMQBA%3D%3D",
                "playlists" => "EgeKAQQoAEABagoQCRAFEAoQAxAE",
                _ => "",
            };
            if !filter_param.is_empty() {
                params["params"] = json!(filter_param);
            }
        }

        self.ytm_post("search", params).await
    }

    // ------------------------------------------------------------------
    // YouTube Data API v3 helper (optional, higher quota)
    // ------------------------------------------------------------------

    /// GET from the YouTube Data API v3 (requires API key).
    async fn yt_api_get(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let api_key = self
            .api_key
            .as_deref()
            .ok_or("no YouTube API key configured")?;

        let url = format!("{YT_API_BASE}/{endpoint}");
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("key", api_key));

        let resp = self
            .client
            .get(&url)
            .query(&query)
            .send()
            .await
            .map_err(|e| format!("yt api {endpoint}: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            info!(endpoint, status, body = %body.chars().take(200).collect::<String>(), "yt_api_error");
            return Err(format!("yt {endpoint}: {status}"));
        }

        resp.json().await.map_err(|e| format!("yt json parse: {e}"))
    }

    // ------------------------------------------------------------------
    // Native stream URL extraction via YTM /player API
    // ------------------------------------------------------------------

    /// Extract audio stream URL natively via the YouTube Music `/player` API.
    ///
    /// Uses the Android Music client context which returns direct audio URLs
    /// without requiring JavaScript signature deciphering. This is the primary
    /// extraction method — no external binary dependency.
    async fn extract_audio_url_native(&self, track_id: &str) -> Result<String, String> {
        let body = json!({
            "videoId": track_id,
            "context": ytm_android_context(),
            "playbackContext": {
                "contentPlaybackContext": {
                    "signatureTimestamp": 20073
                }
            }
        });

        let url = format!("{YTM_API_BASE}/player?prettyPrint=false");
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Origin", "https://music.youtube.com")
            .header("Referer", "https://music.youtube.com/")
            .header(
                "User-Agent",
                "com.google.android.apps.youtube.music/7.27.52 (Linux; U; Android 11) gzip",
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ytm player request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            let snippet: String = body_text.chars().take(300).collect();
            warn!(track_id, status, body = %snippet, "ytm_player_api_error");
            return Err(format!(
                "YouTube player API returned {status} for {track_id}"
            ));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("ytm player json parse: {e}"))?;

        // Check playability status — YouTube may block the video
        let playability = &data["playabilityStatus"];
        let status = playability["status"].as_str().unwrap_or("UNKNOWN");
        if status != "OK" {
            let reason = playability["reason"]
                .as_str()
                .or_else(|| {
                    playability["messages"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("unknown reason");
            warn!(track_id, status, reason, "youtube_not_playable");
            return Err(format!("YouTube track {track_id} not playable: {reason}"));
        }

        // Extract the best audio stream from adaptiveFormats
        let formats = data["streamingData"]["adaptiveFormats"]
            .as_array()
            .ok_or_else(|| format!("no streaming data for {track_id}"))?;

        // Find best audio format: prefer OPUS (audio/webm) at highest bitrate,
        // then fall back to AAC (audio/mp4)
        let mut best_url: Option<&str> = None;
        let mut best_bitrate: u64 = 0;
        let mut best_is_opus = false;

        for fmt in formats {
            let mime = fmt["mimeType"].as_str().unwrap_or("");
            let is_audio = mime.starts_with("audio/");
            if !is_audio {
                continue;
            }

            let bitrate = fmt["bitrate"].as_u64().unwrap_or(0);
            let is_opus = mime.contains("opus");

            // Prefer OPUS; within same codec family, prefer higher bitrate
            let dominated = if best_is_opus && !is_opus {
                true // Don't replace opus with non-opus
            } else if !best_is_opus && is_opus {
                false // Always prefer opus
            } else {
                bitrate <= best_bitrate
            };

            if dominated {
                continue;
            }

            // Prefer direct URL (no cipher); skip formats requiring signature
            if let Some(u) = fmt["url"].as_str() {
                best_url = Some(u);
                best_bitrate = bitrate;
                best_is_opus = is_opus;
            }
        }

        let stream_url =
            best_url.ok_or_else(|| format!("no suitable audio stream found for {track_id}"))?;

        debug!(
            track_id,
            bitrate = best_bitrate,
            opus = best_is_opus,
            "ytm_native_url_extracted"
        );
        Ok(stream_url.to_string())
    }

    // ------------------------------------------------------------------
    // yt-dlp stream URL extraction (fallback)
    // ------------------------------------------------------------------

    /// Extract audio stream URL via yt-dlp subprocess (fallback).
    ///
    /// Used when native `/player` API extraction fails (e.g., geo-restricted
    /// content, age-gated videos). Requires `yt-dlp` installed on PATH.
    async fn extract_audio_url_ytdlp(&self, track_id: &str) -> Result<String, String> {
        let video_url = format!("https://www.youtube.com/watch?v={track_id}");

        let output = tokio::time::timeout(
            Duration::from_secs(YTDLP_TIMEOUT_SECS),
            tokio::process::Command::new("yt-dlp")
                .args([
                    // Prefer a non-segmented HTTPS stream (progressive download).
                    // "bestaudio" alone often picks a DASH format whose URL only
                    // covers the first segment (~50s).
                    "-f",
                    "bestaudio[protocol=https]/bestaudio[protocol=http]/bestaudio",
                    "--get-url",
                    "--no-playlist",
                    "--no-warnings",
                    "-q",
                    &video_url,
                ])
                .output(),
        )
        .await
        .map_err(|_| format!("yt-dlp timeout for {track_id}"))?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "yt-dlp not found — install it with 'pip install yt-dlp' \
                     or 'brew install yt-dlp' for YouTube playback fallback"
                )
            } else {
                format!("yt-dlp exec error: {e}")
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(track_id, stderr = %stderr.chars().take(200).collect::<String>(), "yt_dlp_failed");
            return Err(format!("yt-dlp failed for {track_id}: {stderr}"));
        }

        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if url.is_empty() {
            return Err(format!("yt-dlp returned empty URL for {track_id}"));
        }

        Ok(url)
    }

    /// Extract audio stream URL with native API as primary + yt-dlp fallback.
    async fn extract_audio_url(&self, track_id: &str) -> Result<String, String> {
        // Primary: native YTM /player API (no external dependency)
        match self.extract_audio_url_native(track_id).await {
            Ok(url) => return Ok(url),
            Err(e) => {
                info!(track_id, error = %e, "ytm_native_extraction_failed_trying_ytdlp");
            }
        }

        // Fallback: yt-dlp subprocess
        match self.extract_audio_url_ytdlp(track_id).await {
            Ok(url) => Ok(url),
            Err(ytdlp_err) => {
                warn!(track_id, error = %ytdlp_err, "youtube_all_extraction_methods_failed");
                Err(format!(
                    "YouTube playback failed for track {track_id}: \
                     native API and yt-dlp both failed. \
                     Last error: {ytdlp_err}"
                ))
            }
        }
    }

    // ------------------------------------------------------------------
    // Browse cache
    // ------------------------------------------------------------------

    async fn browse_cache_get(&self, key: &str) -> Option<serde_json::Value> {
        let cache = self.browse_cache.lock().await;
        cache.get(key).and_then(|entry| {
            if entry.created.elapsed() < self.browse_cache_ttl {
                Some(entry.data.clone())
            } else {
                None
            }
        })
    }

    async fn browse_cache_set(&self, key: String, data: serde_json::Value) {
        let mut cache = self.browse_cache.lock().await;
        // Evict expired entries when cache grows large
        if cache.len() > 100 {
            let ttl = self.browse_cache_ttl;
            cache.retain(|_, e| e.created.elapsed() < ttl);
        }
        cache.insert(
            key,
            BrowseCacheEntry {
                data,
                created: Instant::now(),
            },
        );
    }

    // ------------------------------------------------------------------
    // Response parsing helpers
    // ------------------------------------------------------------------

    /// Parse ISO 8601 duration (e.g. "PT4M33S") to milliseconds.
    fn parse_iso_duration(duration: &str) -> u64 {
        // Lazy-init is fine here — Regex::new is lightweight
        let re = Regex::new(r"PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?").unwrap();
        if let Some(caps) = re.captures(duration) {
            let h: u64 = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let m: u64 = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let s: u64 = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            (h * 3600 + m * 60 + s) * 1000
        } else {
            0
        }
    }

    /// Parse "M:SS" or "H:MM:SS" duration string to milliseconds.
    fn parse_colon_duration(duration: &str) -> u64 {
        let parts: Vec<&str> = duration.split(':').collect();
        match parts.len() {
            2 => {
                let m: u64 = parts[0].parse().unwrap_or(0);
                let s: u64 = parts[1].parse().unwrap_or(0);
                (m * 60 + s) * 1000
            }
            3 => {
                let h: u64 = parts[0].parse().unwrap_or(0);
                let m: u64 = parts[1].parse().unwrap_or(0);
                let s: u64 = parts[2].parse().unwrap_or(0);
                (h * 3600 + m * 60 + s) * 1000
            }
            _ => 0,
        }
    }

    /// Pick the best thumbnail URL from a YouTube Data API thumbnails object.
    fn best_thumbnail(thumbnails: &serde_json::Value) -> Option<String> {
        for key in &["maxres", "standard", "high", "medium", "default"] {
            if let Some(url) = thumbnails[key]["url"].as_str() {
                return Some(url.to_string());
            }
        }
        None
    }

    /// Pick the best thumbnail URL from a YouTube Music internal API thumbnails array.
    #[allow(dead_code)]
    fn best_ytm_thumbnail(thumbnails: &serde_json::Value) -> Option<String> {
        thumbnails
            .as_array()
            .and_then(|arr| arr.last())
            .and_then(|t| t["url"].as_str())
            .map(|s| s.to_string())
    }

    fn extract_video_id(item: &serde_json::Value) -> Option<String> {
        item["playlistItemData"]["videoId"]
            .as_str()
            .or_else(|| item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchEndpoint"]["videoId"].as_str())
            .or_else(|| item["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0]["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str())
            .map(|s| s.to_string())
    }

    fn extract_flex_columns(item: &serde_json::Value) -> (String, String) {
        let flex = &item["flexColumns"];
        let title = flex[0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
            .as_array()
            .and_then(|runs| runs.first())
            .and_then(|r| r["text"].as_str())
            .unwrap_or("")
            .to_string();
        let artist = flex[1]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
            .as_array()
            .and_then(|runs| runs.first())
            .and_then(|r| r["text"].as_str())
            .unwrap_or("")
            .to_string();
        (title, artist)
    }

    // ------------------------------------------------------------------
    // Mapping: YouTube Data API v3 → StreamTrack/StreamAlbum/StreamArtist
    // ------------------------------------------------------------------

    /// Map a YouTube Data API video item to StreamTrack.
    fn map_video(item: &serde_json::Value) -> StreamTrack {
        let snippet = &item["snippet"];
        let content = &item["contentDetails"];

        StreamTrack {
            id: item["id"].as_str().unwrap_or("").to_string(),
            title: snippet["title"].as_str().unwrap_or("").into(),
            artist: snippet["channelTitle"].as_str().unwrap_or("").into(),
            album: None,
            album_id: None,
            duration_ms: content["duration"]
                .as_str()
                .map(Self::parse_iso_duration)
                .unwrap_or(0),
            cover_path: Self::best_thumbnail(&snippet["thumbnails"]),
            track_number: None,
            disc_number: None,
            explicit: false,
            quality: Some(StreamQuality {
                codec: "OPUS".into(),
                sample_rate: 48000,
                bit_depth: 16,
                bitrate: Some(128000),
                channels: 2,
            }),
        }
    }

    /// Map a YouTube Data API playlist item to StreamAlbum.
    fn map_playlist_as_album(item: &serde_json::Value) -> StreamAlbum {
        let snippet = &item["snippet"];
        let content = &item["contentDetails"];

        StreamAlbum {
            id: item["id"].as_str().unwrap_or("").into(),
            title: snippet["title"].as_str().unwrap_or("").into(),
            artist: snippet["channelTitle"].as_str().unwrap_or("").into(),
            artist_id: None,
            cover_path: Self::best_thumbnail(&snippet["thumbnails"]),
            year: None,
            track_count: content["itemCount"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    /// Map a YouTube Data API channel item to StreamArtist.
    fn map_channel(item: &serde_json::Value) -> StreamArtist {
        let snippet = &item["snippet"];
        StreamArtist {
            id: item["id"].as_str().unwrap_or("").into(),
            name: snippet["title"].as_str().unwrap_or("").into(),
            image_path: Self::best_thumbnail(&snippet["thumbnails"]),
        }
    }

    // ------------------------------------------------------------------
    // Mapping: YouTube Music internal API → StreamTrack/StreamAlbum/etc.
    // ------------------------------------------------------------------

    /// Map a YouTube Music internal API track/song to StreamTrack.
    ///
    /// The internal API returns tracks in deeply nested flexColumns/runs structures.
    #[allow(dead_code)]
    fn map_ytm_track(item: &serde_json::Value) -> StreamTrack {
        // Try multiple possible response formats (search results vs browse vs playlist)
        let video_id = item["videoId"]
            .as_str()
            .or_else(|| item["playlistItemData"]["videoId"].as_str())
            .or_else(|| {
                // Search result format: navigationEndpoint.watchEndpoint.videoId
                item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]
                    ["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchEndpoint"]
                    ["videoId"]
                    .as_str()
            })
            .unwrap_or("")
            .to_string();

        let title = item["title"]
            .as_str()
            .or_else(|| {
                // Search result flexColumns format
                item["flexColumns"]
                    .as_array()
                    .and_then(|cols| cols.first())
                    .and_then(|col| {
                        col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|run| run["text"].as_str())
                    })
            })
            .unwrap_or("Unknown")
            .to_string();

        // Artist: try direct field, then nested artists array, then flexColumns
        let artist = item["artists"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|a| a["name"].as_str())
            .or_else(|| item["artist"].as_str())
            .or_else(|| {
                // Search result: second flexColumn, first text run
                item["flexColumns"]
                    .as_array()
                    .and_then(|cols| cols.get(1))
                    .and_then(|col| {
                        col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|run| run["text"].as_str())
                    })
            })
            .unwrap_or("Unknown")
            .to_string();

        // Album title from nested album object
        let album = item["album"]
            .as_object()
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from);

        // Duration: try duration_seconds, lengthSeconds, or parse "M:SS" format
        let duration_ms = if let Some(secs) = item["duration_seconds"]
            .as_u64()
            .or_else(|| item["lengthSeconds"].as_u64())
        {
            secs * 1000
        } else if let Some(dur_str) = item["duration"].as_str() {
            Self::parse_colon_duration(dur_str)
        } else {
            0
        };

        // Thumbnails
        let cover_path = Self::best_ytm_thumbnail(&item["thumbnails"])
            .or_else(|| Self::best_ytm_thumbnail(&item["thumbnail"]["thumbnails"]));

        StreamTrack {
            id: video_id,
            title,
            artist,
            album,
            album_id: None,
            duration_ms,
            cover_path,
            track_number: None,
            disc_number: None,
            explicit: item["isExplicit"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality {
                codec: "OPUS".into(),
                sample_rate: 48000,
                bit_depth: 16,
                bitrate: Some(128000),
                channels: 2,
            }),
        }
    }

    /// Map a YouTube Music internal API album/playlist to StreamAlbum.
    #[allow(dead_code)]
    fn map_ytm_album(item: &serde_json::Value) -> StreamAlbum {
        let title = item["title"]
            .as_str()
            .or_else(|| {
                item["flexColumns"]
                    .as_array()
                    .and_then(|cols| cols.first())
                    .and_then(|col| {
                        col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|run| run["text"].as_str())
                    })
            })
            .unwrap_or("Unknown")
            .to_string();

        let artist = item["artists"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|a| a["name"].as_str())
            .or_else(|| item["author"].as_str())
            .unwrap_or("")
            .to_string();

        let browse_id = item["browseId"]
            .as_str()
            .or_else(|| item["playlistId"].as_str())
            .unwrap_or("")
            .to_string();

        let year = item["year"]
            .as_str()
            .and_then(|y| y.parse().ok())
            .or_else(|| item["year"].as_u64().map(|y| y as u32));

        let track_count = item["trackCount"]
            .as_u64()
            .or_else(|| item["count"].as_u64())
            .unwrap_or(0) as u32;

        StreamAlbum {
            id: browse_id,
            title,
            artist,
            artist_id: None,
            cover_path: Self::best_ytm_thumbnail(&item["thumbnails"]),
            year,
            track_count,
            quality: None,
        }
    }

    /// Map a YouTube Music internal API playlist to StreamPlaylist.
    #[allow(dead_code)]
    fn map_ytm_playlist(item: &serde_json::Value) -> StreamPlaylist {
        let playlist_id = item["playlistId"]
            .as_str()
            .or_else(|| item["browseId"].as_str())
            .unwrap_or("")
            .to_string();

        StreamPlaylist {
            id: playlist_id,
            name: item["title"].as_str().unwrap_or("Unknown").into(),
            description: item["description"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map(Into::into),
            cover_path: Self::best_ytm_thumbnail(&item["thumbnails"]),
            track_count: item["count"]
                .as_u64()
                .or_else(|| item["trackCount"].as_u64())
                .unwrap_or(0) as u32,
            owner: item["author"].as_str().map(Into::into),
        }
    }

    // ------------------------------------------------------------------
    // Search response parsing (YouTube Music internal API)
    // ------------------------------------------------------------------

    /// Parse YouTube Music search results into tracks, albums, artists, playlists.
    ///
    /// The internal API response is deeply nested. Results are inside:
    /// `contents.tabbedSearchResultsRenderer.tabs[0].tabRenderer.content
    ///  .sectionListRenderer.contents[].musicShelfRenderer.contents[]`
    fn parse_search_results(&self, data: &serde_json::Value) -> SearchResults {
        let mut tracks = Vec::new();
        let mut albums = Vec::new();
        let mut artists = Vec::new();
        let mut playlists = Vec::new();

        // Navigate to the search results sections
        let sections = data["contents"]["tabbedSearchResultsRenderer"]["tabs"]
            .as_array()
            .and_then(|tabs| tabs.first())
            .and_then(|tab| {
                tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array()
            });

        let sections = match sections {
            Some(s) => s,
            None => {
                return SearchResults {
                    tracks,
                    albums,
                    artists,
                    playlists,
                };
            }
        };

        for section in sections {
            // Support both musicShelfRenderer and musicCardShelfRenderer
            let (shelf, is_card) = if !section["musicShelfRenderer"].is_null() {
                (&section["musicShelfRenderer"], false)
            } else if !section["musicCardShelfRenderer"].is_null() {
                (&section["musicCardShelfRenderer"], true)
            } else {
                continue;
            };

            // For musicCardShelfRenderer, extract the single top result
            if is_card {
                let title_text = shelf["title"]["runs"]
                    .as_array()
                    .and_then(|runs| runs.first())
                    .and_then(|r| r["text"].as_str())
                    .unwrap_or("");
                let subtitle = shelf["subtitle"]["runs"]
                    .as_array()
                    .map(|runs| {
                        runs.iter()
                            .filter_map(|r| r["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let thumb = Self::best_ytm_thumbnail(
                    &shelf["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
                );
                let nav = &shelf["title"]["runs"][0]["navigationEndpoint"];
                let browse_id = nav["browseEndpoint"]["browseId"].as_str().unwrap_or("");
                let video_id = nav["watchEndpoint"]["videoId"].as_str().unwrap_or("");
                let page_type = nav["browseEndpoint"]["browseEndpointContextSupportedConfigs"]
                    ["browseEndpointContextMusicConfig"]["pageType"]
                    .as_str()
                    .unwrap_or("");

                if page_type.contains("ARTIST") || subtitle.to_lowercase().contains("artist") {
                    artists.push(StreamArtist {
                        id: browse_id.to_string(),
                        name: title_text.to_string(),
                        image_path: thumb,
                    });
                } else if page_type.contains("ALBUM") || subtitle.to_lowercase().contains("album") {
                    albums.push(StreamAlbum {
                        id: browse_id.to_string(),
                        title: title_text.to_string(),
                        artist: subtitle.clone(),
                        artist_id: None,
                        cover_path: thumb,
                        year: None,
                        track_count: 0,
                        quality: None,
                    });
                } else if !video_id.is_empty() {
                    tracks.push(StreamTrack {
                        id: video_id.to_string(),
                        title: title_text.to_string(),
                        artist: subtitle.clone(),
                        album: None,
                        album_id: None,
                        duration_ms: 0,
                        cover_path: thumb,
                        track_number: None,
                        disc_number: None,
                        explicit: false,
                        quality: None,
                    });
                }

                // Also parse contents inside the card (related items)
                if let Some(card_contents) = shelf["contents"].as_array() {
                    for item_wrapper in card_contents {
                        let item = &item_wrapper["musicResponsiveListItemRenderer"];
                        if item.is_null() {
                            continue;
                        }
                        if let Some(vid) = Self::extract_video_id(item) {
                            let (title, artist) = Self::extract_flex_columns(item);
                            let thumb = Self::best_ytm_thumbnail(
                                &item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
                            );
                            tracks.push(StreamTrack {
                                id: vid,
                                title,
                                artist,
                                album: None,
                                album_id: None,
                                duration_ms: 0,
                                cover_path: thumb,
                                track_number: None,
                                disc_number: None,
                                explicit: false,
                                quality: None,
                            });
                        }
                    }
                }
                continue;
            }

            let contents = match shelf["contents"].as_array() {
                Some(c) => c,
                None => continue,
            };

            // Determine section type from the title
            let section_title = shelf["title"]["runs"]
                .as_array()
                .and_then(|runs| runs.first())
                .and_then(|r| r["text"].as_str())
                .unwrap_or("");

            let section_type = section_title.to_lowercase();

            for item_wrapper in contents {
                let item = &item_wrapper["musicResponsiveListItemRenderer"];
                if item.is_null() {
                    continue;
                }

                if section_type.contains("song") || section_type.contains("video") {
                    // Extract videoId from the play button endpoint
                    let video_id = item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]
                        ["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchEndpoint"]
                        ["videoId"]
                        .as_str()
                        .or_else(|| {
                            item["playlistItemData"]["videoId"].as_str()
                        });

                    if let Some(vid) = video_id {
                        let flex_cols = item["flexColumns"].as_array();
                        let title = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        let artist_name = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.get(1))
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        // Duration from fixedColumns
                        let duration_str = item["fixedColumns"]
                            .as_array()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFixedColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("0:00");

                        let cover =
                            item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                                .as_array()
                                .and_then(|arr| arr.last())
                                .and_then(|t| t["url"].as_str())
                                .map(String::from);

                        tracks.push(StreamTrack {
                            id: vid.to_string(),
                            title: title.into(),
                            artist: artist_name.into(),
                            album: None,
                            album_id: None,
                            duration_ms: Self::parse_colon_duration(duration_str),
                            cover_path: cover,
                            track_number: None,
                            disc_number: None,
                            explicit: false,
                            quality: Some(StreamQuality {
                                codec: "OPUS".into(),
                                sample_rate: 48000,
                                bit_depth: 16,
                                bitrate: Some(128000),
                                channels: 2,
                            }),
                        });
                    }
                } else if section_type.contains("album") {
                    let browse_id = item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                        .as_str()
                        .unwrap_or("");

                    if !browse_id.is_empty() {
                        let flex_cols = item["flexColumns"].as_array();
                        let title = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        let artist_name = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.get(1))
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("");

                        let cover =
                            item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                                .as_array()
                                .and_then(|arr| arr.last())
                                .and_then(|t| t["url"].as_str())
                                .map(String::from);

                        albums.push(StreamAlbum {
                            id: browse_id.into(),
                            title: title.into(),
                            artist: artist_name.into(),
                            artist_id: None,
                            cover_path: cover,
                            year: None,
                            track_count: 0,
                            quality: None,
                        });
                    }
                } else if section_type.contains("artist") {
                    let browse_id = item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                        .as_str()
                        .unwrap_or("");

                    if !browse_id.is_empty() {
                        let name = item["flexColumns"]
                            .as_array()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        let image =
                            item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                                .as_array()
                                .and_then(|arr| arr.last())
                                .and_then(|t| t["url"].as_str())
                                .map(String::from);

                        artists.push(StreamArtist {
                            id: browse_id.into(),
                            name: name.into(),
                            image_path: image,
                        });
                    }
                } else if section_type.contains("playlist") {
                    let browse_id = item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                        .as_str()
                        .or_else(|| {
                            item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]
                                ["musicPlayButtonRenderer"]["playNavigationEndpoint"]
                                ["watchPlaylistEndpoint"]["playlistId"]
                                .as_str()
                        })
                        .unwrap_or("");

                    if !browse_id.is_empty() {
                        let name = item["flexColumns"]
                            .as_array()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        let cover =
                            item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                                .as_array()
                                .and_then(|arr| arr.last())
                                .and_then(|t| t["url"].as_str())
                                .map(String::from);

                        playlists.push(StreamPlaylist {
                            id: browse_id.into(),
                            name: name.into(),
                            description: None,
                            cover_path: cover,
                            track_count: 0,
                            owner: None,
                        });
                    }
                }
            }
        }

        SearchResults {
            tracks,
            albums,
            artists,
            playlists,
        }
    }

    // ------------------------------------------------------------------
    // Browse endpoints (YouTube Music internal API)
    // ------------------------------------------------------------------

    /// Browse a YouTube Music page (album, artist, playlist).
    async fn ytm_browse(&self, browse_id: &str) -> Result<serde_json::Value, String> {
        let body = json!({
            "browseId": browse_id,
            "context": ytm_context(),
        });
        self.ytm_post("browse", body).await
    }

    /// Get YouTube Music home page sections.
    async fn ytm_get_home(&self) -> Result<serde_json::Value, String> {
        let body = json!({
            "browseId": "FEmusic_home",
            "context": ytm_context(),
        });
        self.ytm_post("browse", body).await
    }

    /// Fetch video details in batch via YouTube Data API v3.
    /// Falls back gracefully if no API key is configured.
    async fn fetch_videos_batch(&self, video_ids: &[String]) -> Vec<StreamTrack> {
        if video_ids.is_empty() {
            return vec![];
        }

        // Try Data API v3 first (more reliable metadata)
        if self.api_key.is_some() {
            let mut tracks = Vec::new();
            for chunk in video_ids.chunks(50) {
                let ids = chunk.join(",");
                if let Ok(data) = self
                    .yt_api_get(
                        "videos",
                        &[("part", "snippet,contentDetails"), ("id", &ids)],
                    )
                    .await
                {
                    if let Some(items) = data["items"].as_array() {
                        for item in items {
                            tracks.push(Self::map_video(item));
                        }
                    }
                }
            }
            return tracks;
        }

        // No API key — return basic tracks with just the video IDs
        video_ids
            .iter()
            .map(|id| StreamTrack {
                id: id.clone(),
                title: String::new(), // Will be populated when track is played
                artist: String::new(),
                album: None,
                album_id: None,
                duration_ms: 0,
                cover_path: Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")),
                track_number: None,
                disc_number: None,
                explicit: false,
                quality: Some(StreamQuality {
                    codec: "OPUS".into(),
                    sample_rate: 48000,
                    bit_depth: 16,
                    bitrate: Some(128000),
                    channels: 2,
                }),
            })
            .collect()
    }

    /// Parse album/playlist tracks from YouTube Music browse response.
    fn parse_browse_tracks(data: &serde_json::Value) -> Vec<StreamTrack> {
        let mut tracks = Vec::new();

        // Album tracks are nested in:
        // contents.singleColumnBrowseResultsRenderer.tabs[0].tabRenderer.content
        //   .sectionListRenderer.contents[0].musicShelfRenderer.contents[]
        let sections = data["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
            .as_array()
            .and_then(|tabs| tabs.first())
            .and_then(|tab| {
                tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array()
            });

        // Also try the twoColumnBrowseResultsRenderer format (newer YTM)
        let sections = sections.or_else(|| {
            data["contents"]["twoColumnBrowseResultsRenderer"]["secondaryContents"]
                ["sectionListRenderer"]["contents"]
                .as_array()
        });

        if let Some(sections) = sections {
            for section in sections {
                let contents = section["musicShelfRenderer"]["contents"]
                    .as_array()
                    .or_else(|| section["musicPlaylistShelfRenderer"]["contents"].as_array());

                if let Some(contents) = contents {
                    for (idx, item_wrapper) in contents.iter().enumerate() {
                        let item = &item_wrapper["musicResponsiveListItemRenderer"];
                        if item.is_null() {
                            continue;
                        }

                        // Extract videoId
                        let video_id = item["playlistItemData"]["videoId"].as_str().or_else(|| {
                            item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]
                                    ["musicPlayButtonRenderer"]["playNavigationEndpoint"]
                                    ["watchEndpoint"]["videoId"]
                                    .as_str()
                        });

                        let video_id = match video_id {
                            Some(vid) => vid.to_string(),
                            None => continue,
                        };

                        let flex_cols = item["flexColumns"].as_array();

                        let title = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("Unknown");

                        let artist_name = flex_cols
                            .as_ref()
                            .and_then(|cols| cols.get(1))
                            .and_then(|col| {
                                col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .unwrap_or("");

                        // Duration from fixedColumns
                        let duration_ms = item["fixedColumns"]
                            .as_array()
                            .and_then(|cols| cols.first())
                            .and_then(|col| {
                                col["musicResponsiveListItemFixedColumnRenderer"]["text"]["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first())
                                    .and_then(|run| run["text"].as_str())
                            })
                            .map(Self::parse_colon_duration)
                            .unwrap_or(0);

                        let cover =
                            item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                                .as_array()
                                .and_then(|arr| arr.last())
                                .and_then(|t| t["url"].as_str())
                                .map(String::from);

                        tracks.push(StreamTrack {
                            id: video_id,
                            title: title.into(),
                            artist: artist_name.into(),
                            album: None,
                            album_id: None,
                            duration_ms,
                            cover_path: cover,
                            track_number: Some((idx + 1) as u32),
                            disc_number: Some(1),
                            explicit: false,
                            quality: Some(StreamQuality {
                                codec: "OPUS".into(),
                                sample_rate: 48000,
                                bit_depth: 16,
                                bitrate: Some(128000),
                                channels: 2,
                            }),
                        });
                    }
                }
            }
        }

        tracks
    }

    /// Parse album header metadata from YouTube Music browse response.
    fn parse_album_header(data: &serde_json::Value) -> Option<StreamAlbum> {
        // Try header.musicImmersiveHeaderRenderer or header.musicDetailHeaderRenderer
        let header = data["header"]["musicImmersiveHeaderRenderer"]
            .as_object()
            .or_else(|| data["header"]["musicDetailHeaderRenderer"].as_object())?;
        let header = serde_json::Value::Object(header.clone());

        let title = header["title"]["runs"]
            .as_array()
            .and_then(|runs| runs.first())
            .and_then(|r| r["text"].as_str())
            .unwrap_or("Unknown");

        let artist = header["subtitle"]["runs"]
            .as_array()
            .and_then(|runs| {
                // Artist is typically the third run (after "Album" and " • ")
                runs.iter()
                    .find(|r| {
                        r["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]
                            ["browseEndpointContextMusicConfig"]["pageType"]
                            .as_str()
                            == Some("MUSIC_PAGE_TYPE_ARTIST")
                    })
                    .and_then(|r| r["text"].as_str())
            })
            .or_else(|| {
                // Fallback: first run in subtitle
                header["subtitle"]["runs"]
                    .as_array()
                    .and_then(|runs| runs.first())
                    .and_then(|r| r["text"].as_str())
            })
            .unwrap_or("");

        let year = header["subtitle"]["runs"].as_array().and_then(|runs| {
            runs.iter().rev().find_map(|r| {
                r["text"]
                    .as_str()
                    .and_then(|t| t.parse::<u32>().ok())
                    .filter(|&y| y >= 1900 && y <= 2100)
            })
        });

        let cover = header["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
            .as_array()
            .and_then(|arr| arr.last())
            .and_then(|t| t["url"].as_str())
            .map(String::from);

        Some(StreamAlbum {
            id: String::new(), // Caller sets this
            title: title.into(),
            artist: artist.into(),
            artist_id: None,
            cover_path: cover,
            year,
            track_count: 0, // Set from tracks count
            quality: None,
        })
    }

    /// Parse artist details from YouTube Music browse response.
    fn parse_artist_header(data: &serde_json::Value) -> Option<StreamArtist> {
        let header = &data["header"]["musicImmersiveHeaderRenderer"];
        if header.is_null() {
            return None;
        }

        let name = header["title"]["runs"]
            .as_array()
            .and_then(|runs| runs.first())
            .and_then(|r| r["text"].as_str())
            .unwrap_or("Unknown");

        let image = header["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
            .as_array()
            .and_then(|arr| arr.last())
            .and_then(|t| t["url"].as_str())
            .map(String::from);

        Some(StreamArtist {
            id: String::new(), // Caller sets this
            name: name.into(),
            image_path: image,
        })
    }

    /// Parse artist's albums from browse response sections.
    fn parse_artist_albums(data: &serde_json::Value) -> Vec<StreamAlbum> {
        let mut albums = Vec::new();

        let sections = data["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
            .as_array()
            .and_then(|tabs| tabs.first())
            .and_then(|tab| {
                tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array()
            });

        if let Some(sections) = sections {
            for section in sections {
                let shelf = &section["musicCarouselShelfRenderer"];
                if shelf.is_null() {
                    continue;
                }

                // Check if this is an albums section
                let section_title =
                    shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|r| r["text"].as_str())
                        .unwrap_or("");

                if !section_title.to_lowercase().contains("album") {
                    continue;
                }

                if let Some(contents) = shelf["contents"].as_array() {
                    for item_wrapper in contents {
                        let item = &item_wrapper["musicTwoRowItemRenderer"];
                        if item.is_null() {
                            continue;
                        }

                        let browse_id = item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                            .as_str()
                            .unwrap_or("");

                        if browse_id.is_empty() {
                            continue;
                        }

                        let title = item["title"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|r| r["text"].as_str())
                            .unwrap_or("Unknown");

                        let year = item["subtitle"]["runs"].as_array().and_then(|runs| {
                            runs.iter().rev().find_map(|r| {
                                r["text"]
                                    .as_str()
                                    .and_then(|t| t.parse::<u32>().ok())
                                    .filter(|&y| y >= 1900 && y <= 2100)
                            })
                        });

                        let cover = item["thumbnailRenderer"]["musicThumbnailRenderer"]
                            ["thumbnail"]["thumbnails"]
                            .as_array()
                            .and_then(|arr| arr.last())
                            .and_then(|t| t["url"].as_str())
                            .map(String::from);

                        albums.push(StreamAlbum {
                            id: browse_id.into(),
                            title: title.into(),
                            artist: String::new(), // Set by caller
                            artist_id: None,
                            cover_path: cover,
                            year,
                            track_count: 0,
                            quality: None,
                        });
                    }
                }
            }
        }

        albums
    }

    /// Parse artist's top tracks from browse response sections.
    fn parse_artist_top_tracks(data: &serde_json::Value) -> Vec<StreamTrack> {
        let sections = data["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
            .as_array()
            .and_then(|tabs| tabs.first())
            .and_then(|tab| {
                tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array()
            });

        if let Some(sections) = sections {
            for section in sections {
                let shelf = &section["musicShelfRenderer"];
                if shelf.is_null() {
                    continue;
                }

                // Check if this is a "songs" / top tracks section
                let section_title = shelf["title"]["runs"]
                    .as_array()
                    .and_then(|runs| runs.first())
                    .and_then(|r| r["text"].as_str())
                    .unwrap_or("");

                if !section_title.to_lowercase().contains("song") {
                    continue;
                }

                return Self::parse_shelf_tracks(shelf);
            }
        }

        Vec::new()
    }

    /// Parse tracks from a musicShelfRenderer.
    fn parse_shelf_tracks(shelf: &serde_json::Value) -> Vec<StreamTrack> {
        let mut tracks = Vec::new();

        let contents = match shelf["contents"].as_array() {
            Some(c) => c,
            None => return tracks,
        };

        for item_wrapper in contents {
            let item = &item_wrapper["musicResponsiveListItemRenderer"];
            if item.is_null() {
                continue;
            }

            let video_id = item["playlistItemData"]["videoId"].as_str().or_else(|| {
                item["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]
                        ["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchEndpoint"]
                        ["videoId"]
                        .as_str()
            });

            let video_id = match video_id {
                Some(vid) => vid.to_string(),
                None => continue,
            };

            let flex_cols = item["flexColumns"].as_array();

            let title = flex_cols
                .as_ref()
                .and_then(|cols| cols.first())
                .and_then(|col| {
                    col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|run| run["text"].as_str())
                })
                .unwrap_or("Unknown");

            let artist_name = flex_cols
                .as_ref()
                .and_then(|cols| cols.get(1))
                .and_then(|col| {
                    col["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|run| run["text"].as_str())
                })
                .unwrap_or("");

            let duration_ms = item["fixedColumns"]
                .as_array()
                .and_then(|cols| cols.first())
                .and_then(|col| {
                    col["musicResponsiveListItemFixedColumnRenderer"]["text"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|run| run["text"].as_str())
                })
                .map(Self::parse_colon_duration)
                .unwrap_or(0);

            let cover = item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
                .as_array()
                .and_then(|arr| arr.last())
                .and_then(|t| t["url"].as_str())
                .map(String::from);

            tracks.push(StreamTrack {
                id: video_id,
                title: title.into(),
                artist: artist_name.into(),
                album: None,
                album_id: None,
                duration_ms,
                cover_path: cover,
                track_number: None,
                disc_number: None,
                explicit: false,
                quality: Some(StreamQuality {
                    codec: "OPUS".into(),
                    sample_rate: 48000,
                    bit_depth: 16,
                    bitrate: Some(128000),
                    channels: 2,
                }),
            });
        }

        tracks
    }

    /// Parse home page sections for featured content.
    fn parse_home_sections(data: &serde_json::Value) -> Vec<(FeaturedSection, Vec<StreamAlbum>)> {
        let mut sections = Vec::new();

        let contents = data["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
            .as_array()
            .and_then(|tabs| tabs.first())
            .and_then(|tab| {
                tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array()
            });

        if let Some(contents) = contents {
            for (idx, section) in contents.iter().enumerate() {
                let carousel = &section["musicCarouselShelfRenderer"];
                if carousel.is_null() {
                    continue;
                }

                let title =
                    carousel["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|r| r["text"].as_str())
                        .unwrap_or("");

                if title.is_empty() {
                    continue;
                }

                let mut albums = Vec::new();
                if let Some(items) = carousel["contents"].as_array() {
                    for item_wrapper in items {
                        let item = &item_wrapper["musicTwoRowItemRenderer"];
                        if item.is_null() {
                            continue;
                        }

                        let browse_id = item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                            .as_str()
                            .unwrap_or("");
                        let playlist_id = item["overlay"]["musicItemThumbnailOverlayRenderer"]
                            ["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]
                            ["watchPlaylistEndpoint"]["playlistId"]
                            .as_str()
                            .unwrap_or("");

                        let item_id = if !playlist_id.is_empty() {
                            playlist_id
                        } else {
                            browse_id
                        };
                        if item_id.is_empty() {
                            continue;
                        }

                        let item_title = item["title"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|r| r["text"].as_str())
                            .unwrap_or("Unknown");

                        let artist_name = item["subtitle"]["runs"]
                            .as_array()
                            .and_then(|runs| runs.first())
                            .and_then(|r| r["text"].as_str())
                            .unwrap_or("");

                        let cover = item["thumbnailRenderer"]["musicThumbnailRenderer"]
                            ["thumbnail"]["thumbnails"]
                            .as_array()
                            .and_then(|arr| arr.last())
                            .and_then(|t| t["url"].as_str())
                            .map(String::from);

                        albums.push(StreamAlbum {
                            id: item_id.into(),
                            title: item_title.into(),
                            artist: artist_name.into(),
                            artist_id: None,
                            cover_path: cover,
                            year: None,
                            track_count: 0,
                            quality: None,
                        });
                    }
                }

                if !albums.is_empty() {
                    sections.push((
                        FeaturedSection {
                            id: format!("ytm-home-{idx}"),
                            name: title.into(),
                        },
                        albums,
                    ));
                }
            }
        }

        sections
    }
}

// ======================================================================
// StreamingService trait implementation
// ======================================================================

#[async_trait::async_trait]
impl StreamingService for YouTubeService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "youtube"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(true)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    /// YouTube works without OAuth. Authenticate is a no-op that returns success.
    /// In the future, Google OAuth could be added for user playlists/library.
    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        self.authenticated = true;
        info!("youtube_authenticated (no OAuth required)");
        Ok(AuthStatus {
            authenticated: true,
            username: Some("YouTube Music".into()),
            subscription: Some("Free".into()),
            ..Default::default()
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.authenticated,
            username: Some("YouTube Music".into()),
            subscription: Some("Free".into()),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        // YouTube doesn't require logout — always authenticated
        Ok(())
    }

    // ------------------------------------------------------------------
    // Search
    // ------------------------------------------------------------------

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        // Try YouTube Music internal API first (richer music-specific results)
        match self.ytm_search(query, None).await {
            Ok(data) => {
                let mut results = self.parse_search_results(&data);
                // Truncate to requested limit
                results.tracks.truncate(limit);
                results.albums.truncate(limit);
                results.artists.truncate(limit);
                results.playlists.truncate(limit);
                debug!(
                    query,
                    tracks = results.tracks.len(),
                    albums = results.albums.len(),
                    artists = results.artists.len(),
                    "youtube_search_ytm"
                );
                return Ok(results);
            }
            Err(e) => {
                info!(query, error = %e, "ytm_search_failed_trying_data_api");
            }
        }

        // Fallback to YouTube Data API v3 (if API key is configured)
        if self.api_key.is_some() {
            let limit_str = limit.min(50).to_string();
            let data = self
                .yt_api_get(
                    "search",
                    &[
                        ("part", "snippet"),
                        ("q", query),
                        ("maxResults", &limit_str),
                        ("type", "video,playlist,channel"),
                    ],
                )
                .await?;

            let mut video_ids: Vec<String> = Vec::new();
            let mut albums = Vec::new();
            let mut artists = Vec::new();

            for item in data["items"].as_array().unwrap_or(&vec![]) {
                let kind = item["id"]["kind"].as_str().unwrap_or("");
                let snippet = &item["snippet"];
                let cover = Self::best_thumbnail(&snippet["thumbnails"]);
                let title = snippet["title"].as_str().unwrap_or("Unknown");

                match kind {
                    "youtube#video" => {
                        if let Some(vid) = item["id"]["videoId"].as_str() {
                            video_ids.push(vid.to_string());
                        }
                    }
                    "youtube#playlist" => {
                        albums.push(StreamAlbum {
                            id: item["id"]["playlistId"].as_str().unwrap_or("").into(),
                            title: title.into(),
                            artist: snippet["channelTitle"].as_str().unwrap_or("").into(),
                            artist_id: None,
                            cover_path: cover,
                            year: None,
                            track_count: 0,
                            quality: None,
                        });
                    }
                    "youtube#channel" => {
                        artists.push(StreamArtist {
                            id: item["id"]["channelId"].as_str().unwrap_or("").into(),
                            name: title.into(),
                            image_path: cover,
                        });
                    }
                    _ => {}
                }
            }

            let tracks = self.fetch_videos_batch(&video_ids).await;

            return Ok(SearchResults {
                tracks,
                albums,
                artists,
                playlists: vec![],
            });
        }

        Err("YouTube search unavailable — no API key and internal API failed".into())
    }

    // ------------------------------------------------------------------
    // Track
    // ------------------------------------------------------------------

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        // Try Data API v3 first for full metadata
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get(
                    "videos",
                    &[("part", "snippet,contentDetails"), ("id", track_id)],
                )
                .await
            {
                if let Some(item) = data["items"].as_array().and_then(|a| a.first()) {
                    return Ok(Self::map_video(item));
                }
            }
        }

        // Fallback: return a basic track with thumbnail
        Ok(StreamTrack {
            id: track_id.into(),
            title: String::new(),
            artist: String::new(),
            album: None,
            album_id: None,
            duration_ms: 0,
            cover_path: Some(format!("https://i.ytimg.com/vi/{track_id}/hqdefault.jpg")),
            track_number: None,
            disc_number: None,
            explicit: false,
            quality: Some(StreamQuality {
                codec: "OPUS".into(),
                sample_rate: 48000,
                bit_depth: 16,
                bitrate: Some(128000),
                channels: 2,
            }),
        })
    }

    // ------------------------------------------------------------------
    // Stream URL (native API + yt-dlp fallback)
    // ------------------------------------------------------------------

    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        // Check cache first
        {
            let cache = self.url_cache.lock().await;
            if let Some(cached) = cache.get(track_id) {
                debug!(track_id, "youtube_stream_url_cached");
                return Ok(StreamUrl {
                    url: cached.url.clone(),
                    mime_type: "audio/webm".into(),
                    quality: StreamQuality {
                        codec: "OPUS".into(),
                        sample_rate: 48000,
                        bit_depth: 16,
                        bitrate: Some(128000),
                        channels: 2,
                    },
                    expires_at: None,
                });
            }
        }

        // Extract via native API (primary) + yt-dlp (fallback)
        let url = self
            .extract_audio_url(track_id)
            .await
            .map_err(|e| TuneError::Streaming(e))?;

        // Determine MIME type from URL
        let mime_type = if url.contains("mime=audio%2Fwebm") || url.contains(".webm") {
            "audio/webm"
        } else if url.contains("mime=audio%2Fmp4") || url.contains(".m4a") {
            "audio/mp4"
        } else {
            "audio/webm" // Default for YouTube
        };

        // Cache the URL
        {
            let mut cache = self.url_cache.lock().await;
            cache.set(
                track_id.to_string(),
                CachedUrl {
                    url: url.clone(),
                    created: Instant::now(),
                },
            );
        }

        info!(track_id, mime_type, "youtube_stream_url_resolved");

        Ok(StreamUrl {
            url,
            mime_type: mime_type.into(),
            quality: StreamQuality {
                codec: "OPUS".into(),
                sample_rate: 48000,
                bit_depth: 16,
                bitrate: Some(128000),
                channels: 2,
            },
            expires_at: None,
        })
    }

    // ------------------------------------------------------------------
    // Album (= YouTube playlist or YTM album)
    // ------------------------------------------------------------------

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        // If it starts with "MPREb_" or similar, it's a YTM album browseId
        if album_id.starts_with("MPRE") || album_id.starts_with("OLAK") {
            if let Ok(data) = self.ytm_browse(album_id).await {
                if let Some(mut album) = Self::parse_album_header(&data) {
                    album.id = album_id.into();
                    let tracks = Self::parse_browse_tracks(&data);
                    album.track_count = tracks.len() as u32;
                    return Ok(album);
                }
            }
        }

        // Try YouTube Data API v3 (playlist)
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get(
                    "playlists",
                    &[("part", "snippet,contentDetails"), ("id", album_id)],
                )
                .await
            {
                if let Some(item) = data["items"].as_array().and_then(|a| a.first()) {
                    return Ok(Self::map_playlist_as_album(item));
                }
            }
        }

        // Fallback: try YTM browse for any ID
        let data = self
            .ytm_browse(album_id)
            .await
            .map_err(|e| TuneError::Streaming(format!("youtube get_album: {e}")))?;

        if let Some(mut album) = Self::parse_album_header(&data) {
            album.id = album_id.into();
            let tracks = Self::parse_browse_tracks(&data);
            album.track_count = tracks.len() as u32;
            return Ok(album);
        }

        Err(TuneError::NotFound(format!(
            "youtube album {album_id} not found"
        )))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // Try YTM browse first (works for YTM album browseIds)
        if let Ok(data) = self.ytm_browse(album_id).await {
            let tracks = Self::parse_browse_tracks(&data);
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }

        // Try Data API (playlist items)
        if self.api_key.is_some() {
            let mut video_ids: Vec<String> = Vec::new();
            let mut page_token: Option<String> = None;

            loop {
                let mut params = vec![
                    ("part", "snippet"),
                    ("playlistId", album_id),
                    ("maxResults", "50"),
                ];
                let page_token_str;
                if let Some(ref token) = page_token {
                    page_token_str = token.clone();
                    params.push(("pageToken", &page_token_str));
                }

                let data = self.yt_api_get("playlistItems", &params).await?;

                if let Some(items) = data["items"].as_array() {
                    for item in items {
                        if let Some(vid) = item["snippet"]["resourceId"]["videoId"].as_str() {
                            video_ids.push(vid.to_string());
                        }
                    }
                }

                page_token = data["nextPageToken"].as_str().map(String::from);
                if page_token.is_none() {
                    break;
                }
            }

            return Ok(self.fetch_videos_batch(&video_ids).await);
        }

        Err(TuneError::Streaming(format!(
            "youtube: could not fetch tracks for album {album_id}"
        )))
    }

    // ------------------------------------------------------------------
    // Artist (= YouTube channel or YTM artist)
    // ------------------------------------------------------------------

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        // Try YTM browse first (richer artist data)
        if let Ok(data) = self.ytm_browse(artist_id).await {
            if let Some(mut artist) = Self::parse_artist_header(&data) {
                artist.id = artist_id.into();
                return Ok(artist);
            }
        }

        // Try Data API v3
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get("channels", &[("part", "snippet"), ("id", artist_id)])
                .await
            {
                if let Some(item) = data["items"].as_array().and_then(|a| a.first()) {
                    return Ok(Self::map_channel(item));
                }
            }
        }

        Err(TuneError::NotFound(format!(
            "youtube artist {artist_id} not found"
        )))
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        // Try YTM browse (artist page has albums section)
        if let Ok(data) = self.ytm_browse(artist_id).await {
            let albums = Self::parse_artist_albums(&data);
            if !albums.is_empty() {
                return Ok(albums);
            }
        }

        // Try Data API v3 (channel playlists)
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get(
                    "playlists",
                    &[
                        ("part", "snippet,contentDetails"),
                        ("channelId", artist_id),
                        ("maxResults", "50"),
                    ],
                )
                .await
            {
                let albums = data["items"]
                    .as_array()
                    .map(|items| items.iter().map(Self::map_playlist_as_album).collect())
                    .unwrap_or_default();
                return Ok(albums);
            }
        }

        Ok(vec![])
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // Try YTM browse (artist page has "Songs" section)
        if let Ok(data) = self.ytm_browse(artist_id).await {
            let tracks = Self::parse_artist_top_tracks(&data);
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }

        // Try Data API v3 (channel's recent videos)
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get(
                    "search",
                    &[
                        ("part", "snippet"),
                        ("channelId", artist_id),
                        ("type", "video"),
                        ("order", "date"),
                        ("maxResults", "20"),
                    ],
                )
                .await
            {
                let video_ids: Vec<String> = data["items"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|item| item["id"]["videoId"].as_str().map(String::from))
                    .collect();
                return Ok(self.fetch_videos_batch(&video_ids).await);
            }
        }

        Ok(vec![])
    }

    // ------------------------------------------------------------------
    // Playlist
    // ------------------------------------------------------------------

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        // Try YTM browse
        if let Ok(data) = self.ytm_browse(playlist_id).await {
            let header = &data["header"];

            // Try musicDetailHeaderRenderer (playlist header format)
            let title = header["musicDetailHeaderRenderer"]["title"]["runs"]
                .as_array()
                .and_then(|runs| runs.first())
                .and_then(|r| r["text"].as_str())
                .or_else(|| {
                    header["musicEditablePlaylistDetailHeaderRenderer"]
                        ["header"]["musicDetailHeaderRenderer"]["title"]["runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                        .and_then(|r| r["text"].as_str())
                })
                .unwrap_or("Unknown");

            let description = header["musicDetailHeaderRenderer"]["description"]["runs"]
                .as_array()
                .and_then(|runs| runs.first())
                .and_then(|r| r["text"].as_str())
                .filter(|d| !d.is_empty())
                .map(String::from);

            let cover =
                header["musicDetailHeaderRenderer"]["thumbnail"]["croppedSquareThumbnailRenderer"]
                    ["thumbnail"]["thumbnails"]
                    .as_array()
                    .and_then(|arr| arr.last())
                    .and_then(|t| t["url"].as_str())
                    .map(String::from);

            let tracks = Self::parse_browse_tracks(&data);

            return Ok(StreamPlaylist {
                id: playlist_id.into(),
                name: title.into(),
                description,
                cover_path: cover,
                track_count: tracks.len() as u32,
                owner: None,
            });
        }

        // Try Data API v3
        if self.api_key.is_some() {
            if let Ok(data) = self
                .yt_api_get(
                    "playlists",
                    &[("part", "snippet,contentDetails"), ("id", playlist_id)],
                )
                .await
            {
                if let Some(item) = data["items"].as_array().and_then(|a| a.first()) {
                    let snippet = &item["snippet"];
                    let content = &item["contentDetails"];
                    return Ok(StreamPlaylist {
                        id: playlist_id.into(),
                        name: snippet["title"].as_str().unwrap_or("").into(),
                        description: snippet["description"]
                            .as_str()
                            .filter(|d| !d.is_empty())
                            .map(Into::into),
                        cover_path: Self::best_thumbnail(&snippet["thumbnails"]),
                        track_count: content["itemCount"].as_u64().unwrap_or(0) as u32,
                        owner: snippet["channelTitle"].as_str().map(Into::into),
                    });
                }
            }
        }

        Err(TuneError::NotFound(format!(
            "youtube playlist {playlist_id} not found"
        )))
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // Try YTM browse first
        if let Ok(data) = self.ytm_browse(playlist_id).await {
            let tracks = Self::parse_browse_tracks(&data);
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }

        // Fallback to album_tracks logic (YouTube playlists use same API)
        self.get_album_tracks(playlist_id).await
    }

    // ------------------------------------------------------------------
    // User collections (no-op without OAuth)
    // ------------------------------------------------------------------

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        // Requires Google OAuth — not implemented for now
        Ok(vec![])
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }

    // ------------------------------------------------------------------
    // Featured / Browse
    // ------------------------------------------------------------------

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        // Check cache
        if let Some(cached) = self.browse_cache_get("home_sections").await {
            if let Ok(sections) = serde_json::from_value::<Vec<FeaturedSection>>(cached) {
                return Ok(sections);
            }
        }

        // Fetch home page
        let data = self
            .ytm_get_home()
            .await
            .map_err(|e| TuneError::Streaming(format!("youtube home: {e}")))?;

        let parsed = Self::parse_home_sections(&data);
        let sections: Vec<FeaturedSection> = parsed.iter().map(|(s, _)| s.clone()).collect();

        // Cache sections list
        if let Ok(sections_json) = serde_json::to_value(&sections) {
            self.browse_cache_set("home_sections".into(), sections_json)
                .await;
        }

        // Cache individual section contents
        for (section, albums) in &parsed {
            if let Ok(albums_json) = serde_json::to_value(albums) {
                self.browse_cache_set(format!("home_section_{}", section.id), albums_json)
                    .await;
            }
        }

        Ok(sections)
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let cache_key = format!("home_section_{section_id}");

        // Check cache
        if let Some(cached) = self.browse_cache_get(&cache_key).await {
            if let Ok(albums) = serde_json::from_value::<Vec<StreamAlbum>>(cached) {
                return Ok(albums);
            }
        }

        // Cache miss — refresh home sections
        let _ = self.get_featured_sections().await?;

        // Try cache again
        if let Some(cached) = self.browse_cache_get(&cache_key).await {
            if let Ok(albums) = serde_json::from_value::<Vec<StreamAlbum>>(cached) {
                return Ok(albums);
            }
        }

        Ok(vec![])
    }

    // ------------------------------------------------------------------
    // Token persistence (no-op for YouTube without OAuth)
    // ------------------------------------------------------------------

    fn save_tokens(&self) -> Option<serde_json::Value> {
        // No tokens to persist for unauthenticated YouTube access.
        // When Google OAuth is added, this will store access/refresh tokens.
        Some(json!({
            "authenticated": true,
            "api_key_configured": self.api_key.is_some(),
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        // YouTube is always authenticated — just restore state
        if tokens["authenticated"].as_bool() == Some(true) {
            self.authenticated = true;
            return true;
        }
        // Even without saved state, YouTube works
        self.authenticated = true;
        true
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn youtube_service_name() {
        let svc = YouTubeService::new();
        assert_eq!(svc.name(), "youtube");
    }

    #[test]
    fn youtube_default_authenticated() {
        let svc = YouTubeService::new();
        assert!(svc.authenticated);
    }

    #[test]
    fn youtube_enabled_default() {
        let svc = YouTubeService::new();
        assert!(svc.enabled());
    }

    #[test]
    fn youtube_set_enabled() {
        let mut svc = YouTubeService::new();
        svc.set_enabled(false);
        assert!(!svc.enabled());
        svc.set_enabled(true);
        assert!(svc.enabled());
    }

    #[test]
    fn parse_iso_duration_basic() {
        assert_eq!(YouTubeService::parse_iso_duration("PT4M33S"), 273_000);
        assert_eq!(YouTubeService::parse_iso_duration("PT1H2M3S"), 3_723_000);
        assert_eq!(YouTubeService::parse_iso_duration("PT30S"), 30_000);
        assert_eq!(YouTubeService::parse_iso_duration("PT5M"), 300_000);
        assert_eq!(YouTubeService::parse_iso_duration("PT0S"), 0);
        assert_eq!(YouTubeService::parse_iso_duration(""), 0);
        assert_eq!(YouTubeService::parse_iso_duration("invalid"), 0);
    }

    #[test]
    fn parse_colon_duration_basic() {
        assert_eq!(YouTubeService::parse_colon_duration("4:33"), 273_000);
        assert_eq!(YouTubeService::parse_colon_duration("1:02:03"), 3_723_000);
        assert_eq!(YouTubeService::parse_colon_duration("0:30"), 30_000);
        assert_eq!(YouTubeService::parse_colon_duration(""), 0);
    }

    #[test]
    fn map_video_basic() {
        let video = json!({
            "id": "dQw4w9WgXcQ",
            "snippet": {
                "title": "Never Gonna Give You Up",
                "channelTitle": "Rick Astley",
                "thumbnails": {
                    "high": {"url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"}
                }
            },
            "contentDetails": {
                "duration": "PT3M32S"
            }
        });
        let track = YouTubeService::map_video(&video);
        assert_eq!(track.id, "dQw4w9WgXcQ");
        assert_eq!(track.title, "Never Gonna Give You Up");
        assert_eq!(track.artist, "Rick Astley");
        assert_eq!(track.duration_ms, 212_000);
        assert!(track.cover_path.is_some());
        let q = track.quality.unwrap();
        assert_eq!(q.codec, "OPUS");
        assert_eq!(q.sample_rate, 48000);
    }

    #[test]
    fn map_playlist_as_album_basic() {
        let playlist = json!({
            "id": "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf",
            "snippet": {
                "title": "Best of Jazz",
                "channelTitle": "Music Channel",
                "thumbnails": {
                    "medium": {"url": "https://i.ytimg.com/vi/xxx/mqdefault.jpg"}
                }
            },
            "contentDetails": {
                "itemCount": 42
            }
        });
        let album = YouTubeService::map_playlist_as_album(&playlist);
        assert_eq!(album.id, "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf");
        assert_eq!(album.title, "Best of Jazz");
        assert_eq!(album.artist, "Music Channel");
        assert_eq!(album.track_count, 42);
    }

    #[test]
    fn map_channel_basic() {
        let channel = json!({
            "id": "UCxxxxx",
            "snippet": {
                "title": "Miles Davis",
                "thumbnails": {
                    "default": {"url": "https://yt3.ggpht.com/xxx"}
                }
            }
        });
        let artist = YouTubeService::map_channel(&channel);
        assert_eq!(artist.id, "UCxxxxx");
        assert_eq!(artist.name, "Miles Davis");
        assert!(artist.image_path.is_some());
    }

    #[test]
    fn best_thumbnail_selection() {
        let thumbnails = json!({
            "default": {"url": "https://default.jpg"},
            "medium": {"url": "https://medium.jpg"},
            "high": {"url": "https://high.jpg"},
        });
        assert_eq!(
            YouTubeService::best_thumbnail(&thumbnails).as_deref(),
            Some("https://high.jpg")
        );
    }

    #[test]
    fn best_thumbnail_maxres_priority() {
        let thumbnails = json!({
            "default": {"url": "https://default.jpg"},
            "maxres": {"url": "https://maxres.jpg"},
            "high": {"url": "https://high.jpg"},
        });
        assert_eq!(
            YouTubeService::best_thumbnail(&thumbnails).as_deref(),
            Some("https://maxres.jpg")
        );
    }

    #[test]
    fn best_ytm_thumbnail_last() {
        let thumbnails = json!([
            {"url": "https://small.jpg", "width": 60},
            {"url": "https://medium.jpg", "width": 226},
            {"url": "https://large.jpg", "width": 544},
        ]);
        assert_eq!(
            YouTubeService::best_ytm_thumbnail(&thumbnails).as_deref(),
            Some("https://large.jpg")
        );
    }

    #[test]
    fn save_tokens_always_some() {
        let svc = YouTubeService::new();
        let tokens = svc.save_tokens();
        assert!(tokens.is_some());
        let t = tokens.unwrap();
        assert_eq!(t["authenticated"], true);
    }

    #[test]
    fn restore_tokens_basic() {
        let mut svc = YouTubeService::new();
        svc.authenticated = false;
        let tokens = json!({"authenticated": true});
        assert!(svc.restore_tokens(&tokens));
        assert!(svc.authenticated);
    }

    #[test]
    fn restore_tokens_even_empty() {
        let mut svc = YouTubeService::new();
        svc.authenticated = false;
        let tokens = json!({});
        // YouTube is always authenticated
        assert!(svc.restore_tokens(&tokens));
        assert!(svc.authenticated);
    }

    #[test]
    fn url_cache_basic() {
        let mut cache = UrlCache::new(3600);
        assert!(cache.get("abc").is_none());

        cache.set(
            "abc".into(),
            CachedUrl {
                url: "https://example.com/stream".into(),
                created: Instant::now(),
            },
        );
        assert!(cache.get("abc").is_some());
        assert_eq!(cache.get("abc").unwrap().url, "https://example.com/stream");
    }

    #[test]
    fn url_cache_expired() {
        let mut cache = UrlCache::new(0); // TTL = 0, immediately expired
        cache.set(
            "abc".into(),
            CachedUrl {
                url: "https://example.com/stream".into(),
                created: Instant::now() - Duration::from_secs(1),
            },
        );
        assert!(cache.get("abc").is_none());
    }

    #[tokio::test]
    async fn auth_status_default() {
        let svc = YouTubeService::new();
        let status = svc.auth_status().await;
        assert!(status.authenticated);
        assert_eq!(status.username.as_deref(), Some("YouTube Music"));
    }

    #[tokio::test]
    async fn logout_noop() {
        let mut svc = YouTubeService::new();
        assert!(svc.logout().await.is_ok());
        // Still authenticated after logout (YouTube doesn't require auth)
        assert!(svc.authenticated);
    }
}
