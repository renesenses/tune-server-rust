use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::*;

use crate::TuneError;
const API_BASE: &str = "https://api.tidal.com/v1";
const AUTH_BASE: &str = "https://auth.tidal.com/v1/oauth2";
const LOGIN_BASE: &str = "https://login.tidal.com";
/// tidalapi client — Device Code + refresh + playback work.
/// DEvir confirmed playback works (DASH manifest). Tidal returns
/// application/dash+xml for Hi-Res which needs XML parsing.
const CLIENT_ID: &str = "fX2JxdmntZWK0ixT";
const CLIENT_SECRET: &str = "1Nn9AfDAjxrgJFJbKNWLeAyKGVGmINuXPPLHVXAvxAg=";
const REDIRECT_URI: &str = "http://localhost:8888/api/v1/streaming/tidal/callback";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: u64,
    #[serde(default)]
    user_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceAuthResponse {
    #[serde(alias = "deviceCode")]
    device_code: String,
    #[serde(alias = "userCode")]
    user_code: String,
    #[serde(alias = "verificationUri")]
    verification_uri: String,
    #[serde(alias = "verificationUriComplete")]
    verification_uri_complete: String,
    #[serde(alias = "expiresIn")]
    expires_in: u64,
    interval: u64,
}

/// Mutable token state behind a Mutex so `&self` methods can refresh on 401.
struct TokenState {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_expires: Option<Instant>,
}

/// PKCE state stored while waiting for the OAuth callback.
#[derive(Debug, Clone)]
struct PkceState {
    code_verifier: String,
    state: String,
    started_at: Instant,
}

/// Cached stream URL with its resolved quality metadata.
#[derive(Clone)]
struct CachedUrl {
    url: String,
    mime_type: String,
    codec: String,
    sample_rate: u32,
    bit_depth: u16,
    bitrate: Option<u32>,
    created: Instant,
}

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
        if self.entries.len() > 1000 {
            let ttl = self.ttl;
            self.entries.retain(|_, e| e.created.elapsed() < ttl);
        }
        self.entries.insert(key, entry);
    }
}

struct FeaturedCache {
    sections: Vec<(FeaturedSection, Vec<StreamAlbum>)>,
    fetched_at: Instant,
}

pub struct TidalService {
    client: Client,
    tokens: Mutex<TokenState>,
    country_code: String,
    quality: String,
    username: Option<String>,
    user_id: Option<u64>,
    subscription: Option<String>,
    url_cache: Arc<Mutex<UrlCache>>,
    /// PKCE OAuth2 pending state (primary auth flow).
    pending_pkce: Option<PkceState>,
    /// Legacy Device Code pending state (fallback auth flow).
    pending_device_auth: Option<DeviceAuthResponse>,
    device_auth_started: Option<Instant>,
    featured_cache: Option<FeaturedCache>,
    enabled_override: Option<bool>,
}

impl Default for TidalService {
    fn default() -> Self {
        Self::new()
    }
}

impl TidalService {
    pub fn new() -> Self {
        Self {
            client: crate::http::client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            tokens: Mutex::new(TokenState {
                access_token: None,
                refresh_token: None,
                token_expires: None,
            }),
            country_code: "US".into(),
            quality: "HI_RES_LOSSLESS".into(),
            username: None,
            user_id: None,
            subscription: None,
            url_cache: Arc::new(Mutex::new(UrlCache::new(240))),
            pending_pkce: None,
            pending_device_auth: None,
            device_auth_started: None,
            featured_cache: None,
            enabled_override: None,
        }
    }

    /// Download all DASH segments (init + media) and concatenate them into a
    /// single fMP4 file on disk. Returns the path to the temp file.
    ///
    /// Tidal Hi-Res FLAC (24-bit) is delivered as fMP4 via DASH SegmentTemplate
    /// with a SegmentTimeline. The init segment contains the MP4 header (ftyp +
    /// moov boxes) and each media segment contains moof+mdat with FLAC frames.
    /// All segments must be concatenated in order to form a valid ISO BMFF file
    /// that symphonia's IsoMp4Reader can decode.
    async fn download_dash_segments(
        &self,
        segments: &DashSegmentInfo,
        track_id: &str,
    ) -> Result<String, String> {
        let start = Instant::now();
        let total = segments.segment_count;
        info!(
            track_id,
            init_url = segments.init_url.as_str(),
            segment_count = total,
            start_number = segments.start_number,
            "tidal_dash_multi_segment_download_starting"
        );

        // Download init segment
        let init_data = self
            .client
            .get(&segments.init_url)
            .send()
            .await
            .map_err(|e| format!("dash init download: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("dash init read: {e}"))?;

        if init_data.len() < 8 {
            return Err(format!(
                "dash init segment too small: {} bytes",
                init_data.len()
            ));
        }
        debug!(
            track_id,
            init_bytes = init_data.len(),
            "tidal_dash_init_segment_downloaded"
        );

        // Create temp file and write init segment
        let tmp_path = std::env::temp_dir()
            .join(format!("tune-dash-{}.mp4", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();

        {
            use std::io::Write;
            let mut file =
                std::fs::File::create(&tmp_path).map_err(|e| format!("tmp create: {e}"))?;
            file.write_all(&init_data)
                .map_err(|e| format!("tmp write init: {e}"))?;
        }

        // Download media segments sequentially and append to file.
        // Sequential to avoid overwhelming Tidal CDN and to maintain order.
        // For a typical 3-4 min track at 96kHz/24bit, there are ~54 segments
        // of ~4s each, total ~30-50MB. Each segment is ~500KB-1MB.
        let mut total_bytes = init_data.len() as u64;
        let mut failed_segments = 0u32;

        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&tmp_path)
                .map_err(|e| format!("tmp reopen: {e}"))?;

            for i in 0..total {
                let seg_number = segments.start_number + i;
                let seg_url = segments
                    .media_template
                    .replace("$Number$", &seg_number.to_string());

                match self.client.get(&seg_url).send().await {
                    Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                        Ok(data) => {
                            file.write_all(&data)
                                .map_err(|e| format!("tmp write seg {seg_number}: {e}"))?;
                            total_bytes += data.len() as u64;
                        }
                        Err(e) => {
                            warn!(
                                track_id,
                                segment = seg_number,
                                error = %e,
                                "tidal_dash_segment_read_failed"
                            );
                            failed_segments += 1;
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        warn!(
                            track_id,
                            segment = seg_number,
                            status,
                            "tidal_dash_segment_http_error"
                        );
                        failed_segments += 1;
                    }
                    Err(e) => {
                        warn!(
                            track_id,
                            segment = seg_number,
                            error = %e,
                            "tidal_dash_segment_fetch_error"
                        );
                        failed_segments += 1;
                    }
                }
            }
        }

        let elapsed = start.elapsed();

        if failed_segments > 0 && failed_segments >= total {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!(
                "all {total} DASH segments failed to download for track {track_id}"
            ));
        }

        info!(
            track_id,
            total_bytes,
            segment_count = total,
            failed_segments,
            elapsed_ms = elapsed.as_millis() as u64,
            path = tmp_path.as_str(),
            "tidal_dash_multi_segment_download_complete"
        );

        Ok(tmp_path)
    }

    /// Create a TidalService with a specific quality setting from config.
    /// Valid values: "HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"
    pub fn with_quality(quality: &str) -> Self {
        let mut svc = Self::new();
        svc.quality = quality.into();
        svc
    }

    /// Get current access token from the token state.
    async fn get_access_token(&self) -> Result<String, String> {
        let ts = self.tokens.lock().await;
        ts.access_token
            .clone()
            .ok_or_else(|| "not authenticated".into())
    }

    // ---- PKCE helpers ----

    /// Generate a cryptographically random code verifier (43-128 URL-safe chars).
    fn generate_code_verifier() -> String {
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        // Use multiple UUIDs for entropy (each v4 UUID has 122 bits of randomness)
        let mut result = String::with_capacity(128);
        for _ in 0..3 {
            let uuid = uuid::Uuid::new_v4();
            let bytes = uuid.as_bytes();
            for &b in bytes {
                if result.len() >= 128 {
                    break;
                }
                result.push(CHARSET[(b as usize) % CHARSET.len()] as char);
            }
        }
        // Ensure minimum length of 43 (3 UUIDs * 16 bytes = 48 chars, always enough)
        result
    }

    /// Generate the S256 code challenge from a code verifier.
    fn generate_code_challenge(verifier: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        base64url_encode(&hash)
    }

    /// Generate a random state parameter for CSRF protection.
    fn generate_state() -> String {
        uuid::Uuid::new_v4().to_string().replace('-', "")
    }

    /// Build the Tidal authorization URL for PKCE flow.
    fn build_authorize_url(code_challenge: &str, state: &str) -> String {
        format!(
            "{LOGIN_BASE}/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={code_challenge}&code_challenge_method=S256&state={state}",
            urlencoding::encode(CLIENT_ID),
            urlencoding::encode(REDIRECT_URI),
            urlencoding::encode(
                "user.read playback collection.read collection.write playlists.read playlists.write entitlements.read search.read"
            ),
        )
    }

    /// Exchange an authorization code for tokens (PKCE flow).
    async fn exchange_code_for_tokens(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, String> {
        let resp = self
            .client
            .post(format!("{AUTH_BASE}/token"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("code", code),
                ("redirect_uri", REDIRECT_URI),
                ("grant_type", "authorization_code"),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(|e| format!("pkce token exchange: {e}"))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;

        if status != 200 {
            warn!(status, body = %body, "tidal_pkce_token_exchange_failed");
            return Err(format!("tidal PKCE token exchange failed: {status} {body}"));
        }

        let token: TokenResponse = serde_json::from_str(&body).map_err(|e| {
            warn!(error = %e, body = %body, "tidal_pkce_token_parse_failed");
            format!("token parse: {e}")
        })?;
        Ok(token)
    }

    /// Handle the OAuth callback after user authorizes in browser.
    /// Called from the `/api/v1/streaming/tidal/callback` route.
    pub async fn handle_oauth_callback(
        &mut self,
        code: &str,
        state: &str,
    ) -> Result<AuthStatus, String> {
        let pkce = self
            .pending_pkce
            .take()
            .ok_or("no pending PKCE auth — start authentication first")?;

        // Verify state matches (CSRF protection)
        if pkce.state != state {
            // Put it back so user can retry
            self.pending_pkce = Some(pkce);
            return Err("state mismatch — possible CSRF attack".into());
        }

        // Check if PKCE flow has expired (5 minutes max)
        if pkce.started_at.elapsed() > Duration::from_secs(300) {
            return Err("PKCE authorization expired — please restart authentication".into());
        }

        let token = self
            .exchange_code_for_tokens(code, &pkce.code_verifier)
            .await?;
        let access_token_clone = token.access_token.clone();
        {
            let mut ts = self.tokens.lock().await;
            ts.access_token = Some(token.access_token);
            ts.refresh_token = token.refresh_token;
            ts.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
        }
        self.user_id = token
            .user_id
            .or_else(|| Self::extract_uid_from_jwt(&access_token_clone));
        self.country_code = "FR".into();

        info!(user_id = ?self.user_id, "tidal_pkce_authenticated");

        // Fetch username and subscription info
        self.refresh_user_info().await;

        Ok(self.auth_status().await)
    }

    /// Attempt to refresh the access token using the refresh_token.
    /// Returns Ok(true) if refresh succeeded, Ok(false) if no refresh_token available,
    /// Err if the refresh call itself failed (token revoked, etc.).
    async fn do_refresh_token(&self) -> Result<bool, String> {
        let refresh_token = {
            let ts = self.tokens.lock().await;
            match ts.refresh_token.clone() {
                Some(rt) => rt,
                None => return Ok(false),
            }
        };

        info!("tidal_auto_refresh: attempting token refresh after 401");

        let resp = self
            .client
            .post(format!("{AUTH_BASE}/token"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("refresh_token", refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| format!("refresh: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            warn!(status, body = %body, "tidal_refresh_token_rejected");

            let permanently_invalid = is_refresh_permanently_invalid(status, &body);

            if permanently_invalid {
                warn!(
                    "tidal_refresh_permanently_failed — clearing all tokens, reconnection required"
                );
                let mut ts = self.tokens.lock().await;
                ts.refresh_token = None;
                ts.access_token = None;
                ts.token_expires = None;
            }
            return Err("refresh token rejected".into());
        }

        let token: TokenResponse = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        {
            let mut ts = self.tokens.lock().await;
            ts.access_token = Some(token.access_token);
            if let Some(rt) = token.refresh_token {
                ts.refresh_token = Some(rt);
            }
            ts.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
        }
        info!("tidal_token_refreshed_after_401");
        Ok(true)
    }

    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let token = self.get_access_token().await?;
        let url = format!("{API_BASE}{path}");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&[("countryCode", &self.country_code)])
            .send()
            .await
            .map_err(|e| format!("tidal api: {e}"))?;

        if resp.status() == 401 {
            // Token expired — try to refresh and retry once
            match self.do_refresh_token().await {
                Ok(true) => {
                    let new_token = self.get_access_token().await?;
                    let retry_resp = self
                        .client
                        .get(&url)
                        .header("Authorization", format!("Bearer {new_token}"))
                        .query(&[("countryCode", &self.country_code)])
                        .send()
                        .await
                        .map_err(|e| format!("tidal api retry: {e}"))?;
                    if retry_resp.status() == 401 {
                        return Err("token expired after refresh".into());
                    }
                    return retry_resp
                        .json()
                        .await
                        .map_err(|e| format!("tidal json: {e}"));
                }
                Ok(false) => return Err("token expired, no refresh token".into()),
                Err(e) => return Err(format!("token expired, refresh failed: {e}")),
            }
        }

        resp.json().await.map_err(|e| format!("tidal json: {e}"))
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let tags = item["mediaMetadata"]["tags"].as_array();
        let is_hires = tags
            .map(|t| {
                t.iter().any(|v| {
                    let s = v.as_str().unwrap_or("");
                    s == "HIRES_LOSSLESS" || s == "MQA"
                })
            })
            .unwrap_or(false);
        let audio_quality = item["audioQuality"].as_str().unwrap_or(if is_hires {
            "HI_RES_LOSSLESS"
        } else {
            "LOSSLESS"
        });
        let (sample_rate, bit_depth) = match audio_quality {
            "HI_RES_LOSSLESS" => (96000, 24),
            "HI_RES" => (48000, 24),
            _ => (44100, 16),
        };

        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"]
                .as_str()
                .or_else(|| {
                    item["artists"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|a| a["name"].as_str())
                })
                .unwrap_or("")
                .into(),
            album: item["album"]["title"].as_str().map(Into::into),
            album_id: item["album"]["id"].as_u64().map(|id| id.to_string()),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_path: item["album"]["cover"].as_str().map(|c| {
                format!(
                    "https://resources.tidal.com/images/{}/640x640.jpg",
                    c.replace('-', "/")
                )
            }),
            track_number: item["trackNumber"].as_u64().map(|n| n as u32),
            disc_number: item["volumeNumber"].as_u64().map(|n| n as u32),
            explicit: item["explicit"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate,
                bit_depth,
                bitrate: None,
                channels: 2,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"]
                .as_str()
                .or_else(|| {
                    item["artists"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|a| a["name"].as_str())
                })
                .unwrap_or("")
                .into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_path: item["cover"].as_str().map(|c| {
                format!(
                    "https://resources.tidal.com/images/{}/640x640.jpg",
                    c.replace('-', "/")
                )
            }),
            year: item["releaseDate"]
                .as_str()
                .and_then(|d| d.get(..4)?.parse().ok()),
            track_count: item["numberOfTracks"].as_u64().unwrap_or(0) as u32,
            quality: {
                let tags = item["mediaMetadata"]["tags"].as_array();
                let is_hires = tags.map_or(false, |t| {
                    t.iter()
                        .any(|v| v.as_str().map_or(false, |s| s.contains("HIRES")))
                });
                let aq = item["audioQuality"].as_str().unwrap_or("");
                if !aq.is_empty() || is_hires {
                    Some(StreamQuality {
                        codec: "FLAC".into(),
                        sample_rate: if is_hires { 96000 } else { 44100 },
                        bit_depth: if is_hires { 24 } else { 16 },
                        bitrate: None,
                        channels: 2,
                    })
                } else {
                    None
                }
            },
        }
    }

    async fn fetch_playback_info(
        &self,
        track_id: &str,
        quality: &str,
    ) -> Result<serde_json::Value, String> {
        let token = self.get_access_token().await?;
        let url = format!("{API_BASE}/tracks/{track_id}/playbackinfopostpaywall");
        let params = [
            ("audioquality", quality),
            ("playbackmode", "STREAM"),
            ("assetpresentation", "FULL"),
            ("countryCode", self.country_code.as_str()),
        ];

        // Use a per-request timeout of 15 seconds (longer than the default
        // client timeout of 30s for the overall request including redirects).
        // Retry once on timeout — Tidal CDN can be slow to resolve stream
        // URLs, especially for Hi-Res DASH manifests (DEvir QA B-07).
        let mut last_err = String::new();
        let mut resp = None;
        for attempt in 0..2u8 {
            match self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .query(&params)
                .timeout(Duration::from_secs(15))
                .send()
                .await
            {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    let is_timeout = e.is_timeout() || e.is_connect();
                    last_err = format!("stream url: {e}");
                    if is_timeout && attempt == 0 {
                        warn!(
                            track_id,
                            quality,
                            attempt,
                            error = %e,
                            "tidal_playback_info_timeout_retrying"
                        );
                        continue;
                    }
                    return Err(last_err);
                }
            }
        }
        let resp = resp.ok_or(last_err)?;

        if resp.status() == 429 {
            return Err("tidal rate limited".into());
        }

        if resp.status() == 401 {
            // Token expired — try to refresh and retry once
            match self.do_refresh_token().await {
                Ok(true) => {
                    let new_token = self.get_access_token().await?;
                    let retry_resp = self
                        .client
                        .get(&url)
                        .header("Authorization", format!("Bearer {new_token}"))
                        .query(&params)
                        .send()
                        .await
                        .map_err(|e| format!("stream url retry: {e}"))?;
                    if retry_resp.status() == 429 {
                        return Err("tidal rate limited".into());
                    }
                    let status = retry_resp.status().as_u16();
                    let body = retry_resp
                        .text()
                        .await
                        .map_err(|e| format!("read body: {e}"))?;
                    if status != 200 {
                        info!(track_id, quality, status, body = %body, "tidal_playback_info_error_after_refresh");
                        return Err(format!("tidal playback {status}: {body}"));
                    }
                    return serde_json::from_str(&body).map_err(|e| format!("parse: {e}"));
                }
                Ok(false) => return Err("token expired, no refresh token".into()),
                Err(e) => return Err(format!("token expired, refresh failed: {e}")),
            }
        }

        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
        if status != 200 {
            info!(track_id, quality, status, body = %body, "tidal_playback_info_error");
            return Err(format!("tidal playback {status}: {body}"));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))?;

        // Log key fields from the Tidal playback response (without the manifest
        // blob which is large) — this is essential for diagnosing quality issues.
        debug!(
            track_id,
            requested_quality = quality,
            returned_quality = parsed["audioQuality"].as_str().unwrap_or("?"),
            codec = parsed["codec"].as_str().unwrap_or("?"),
            manifest_mime = parsed["manifestMimeType"].as_str().unwrap_or("?"),
            bit_depth = ?parsed["bitDepth"].as_u64(),
            sample_rate = ?parsed["sampleRate"].as_u64(),
            "tidal_playback_info_raw"
        );
        Ok(parsed)
    }

    fn parse_quality_metadata(data: &serde_json::Value, audio_quality: &str) -> (u32, u16) {
        if let Some(bit_depth) = data["bitDepth"].as_u64() {
            let sample_rate = data["sampleRate"].as_u64().unwrap_or(44100) as u32;
            return (sample_rate, bit_depth as u16);
        }
        match audio_quality {
            "HI_RES_LOSSLESS" => (96000, 24),
            "HI_RES" => (48000, 24),
            "LOSSLESS" => (44100, 16),
            _ => (44100, 16),
        }
    }

    async fn refresh_user_info(&mut self) {
        if let Ok(me) = self.api_get("/users/me").await {
            self.username = me["username"].as_str().map(Into::into);
            if let Some(cc) = me["countryCode"].as_str() {
                self.country_code = cc.into();
            }
            info!(username = ?self.username, country = %self.country_code, "tidal_user_refreshed");
        }
        // Fetch subscription type for quality diagnostics
        if let Some(uid) = self.user_id {
            if let Ok(sub) = self.api_get(&format!("/users/{uid}/subscription")).await {
                let sub_type = sub["subscription"]["type"]
                    .as_str()
                    .or_else(|| sub["type"].as_str())
                    .unwrap_or("unknown");
                self.subscription = Some(sub_type.into());
                let highest = sub["subscription"]["highestSoundQuality"]
                    .as_str()
                    .or_else(|| sub["highestSoundQuality"].as_str())
                    .unwrap_or("unknown");
                info!(
                    subscription = sub_type,
                    highest_quality = highest,
                    "tidal_subscription_info"
                );
            }
        }
    }

    fn extract_uid_from_jwt(token: &str) -> Option<u64> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() < 2 {
            return None;
        }
        let payload = parts[1];
        let decoded = base64_decode_url(payload).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        claims["uid"].as_u64()
    }

    async fn api_post_form(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let token = self.get_access_token().await?;
        let url = format!("{API_BASE}{path}");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&[("countryCode", &self.country_code)])
            .form(form)
            .send()
            .await
            .map_err(|e| format!("tidal post: {e}"))?;

        if resp.status() == 401 {
            if let Ok(true) = self.do_refresh_token().await {
                let new_token = self.get_access_token().await?;
                let retry_resp = self
                    .client
                    .post(&url)
                    .header("Authorization", format!("Bearer {new_token}"))
                    .query(&[("countryCode", &self.country_code)])
                    .form(form)
                    .send()
                    .await
                    .map_err(|e| format!("tidal post retry: {e}"))?;
                if !retry_resp.status().is_success() {
                    let status = retry_resp.status().as_u16();
                    let body = retry_resp.text().await.unwrap_or_default();
                    return Err(format!("tidal {path}: {status} {body}"));
                }
                return retry_resp.json().await.or_else(|_| Ok(json!({"ok": true})));
            }
            return Err(format!("tidal {path}: 401 token expired"));
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("tidal {path}: {status} {body}"));
        }
        resp.json().await.or_else(|_| Ok(json!({"ok": true})))
    }

    async fn api_delete(&self, path: &str) -> Result<(), String> {
        let token = self.get_access_token().await?;
        let url = format!("{API_BASE}{path}");
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&[("countryCode", &self.country_code)])
            .send()
            .await
            .map_err(|e| format!("tidal delete: {e}"))?;

        if resp.status() == 401 {
            if let Ok(true) = self.do_refresh_token().await {
                let new_token = self.get_access_token().await?;
                let retry_resp = self
                    .client
                    .delete(&url)
                    .header("Authorization", format!("Bearer {new_token}"))
                    .query(&[("countryCode", &self.country_code)])
                    .send()
                    .await
                    .map_err(|e| format!("tidal delete retry: {e}"))?;
                if !retry_resp.status().is_success() {
                    let status = retry_resp.status().as_u16();
                    return Err(format!("tidal DELETE {path}: {status}"));
                }
                return Ok(());
            }
            return Err(format!("tidal DELETE {path}: 401 token expired"));
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(format!("tidal DELETE {path}: {status}"));
        }
        Ok(())
    }

    fn map_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["uuid"].as_str().unwrap_or("").into(),
            name: item["title"].as_str().unwrap_or("").into(),
            description: item["description"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map(Into::into),
            cover_path: item["squareImage"]
                .as_str()
                .or_else(|| item["image"].as_str())
                .map(|c| {
                    if c.starts_with("http") {
                        c.to_string()
                    } else {
                        format!(
                            "https://resources.tidal.com/images/{}/640x640.jpg",
                            c.replace('-', "/")
                        )
                    }
                }),
            track_count: item["numberOfTracks"].as_u64().unwrap_or(0) as u32,
            owner: item["creator"]["name"].as_str().map(Into::into),
        }
    }

    fn map_genre(item: &serde_json::Value) -> StreamGenre {
        StreamGenre {
            id: item["path"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children: item["hasSubgenres"]
                .as_bool()
                .or_else(|| item["subGenres"].as_array().map(|a| !a.is_empty()))
                .unwrap_or(false),
            image_url: item["image"].as_str().map(Into::into),
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["picture"].as_str().map(|p| {
                format!(
                    "https://resources.tidal.com/images/{}/480x480.jpg",
                    p.replace('-', "/")
                )
            }),
            bio: None,
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for TidalService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(true)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        // If already authenticated with valid tokens, return current status
        {
            let ts = self.tokens.lock().await;
            if ts.access_token.is_some() && ts.refresh_token.is_some() {
                let token_ok = ts.token_expires.map(|t| Instant::now() < t).unwrap_or(true); // no expiry tracked = assume OK
                if token_ok {
                    drop(ts);
                    return Ok(self.auth_status().await);
                }
            }
        }

        // --- Handle PKCE callback (code + state from OAuth redirect) ---
        if let Some(code) = credentials.get("code").and_then(|v| v.as_str()) {
            let state = credentials
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Ok(self.handle_oauth_callback(code, state).await?);
        }

        // --- Device Code flow (legacy fallback, explicitly requested) ---
        if credentials.get("device_flow").and_then(|v| v.as_bool()) == Some(true) {
            let resp = self
                .client
                .post(format!("{AUTH_BASE}/device_authorization"))
                .form(&[("client_id", CLIENT_ID), ("scope", "r_usr w_usr w_sub")])
                .send()
                .await
                .map_err(|e| format!("device auth: {e}"))?;

            let status_code = resp.status().as_u16();
            let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
            if status_code != 200 {
                warn!(status = status_code, body = %body, "tidal_device_authorization_failed");
                return Err(format!("device authorization failed: {status_code} {body}").into());
            }

            let device_auth: DeviceAuthResponse =
                serde_json::from_str(&body).map_err(|e| format!("parse device auth: {e}"))?;
            info!(
                user_code = %device_auth.user_code,
                uri = %device_auth.verification_uri_complete,
                expires_in = device_auth.expires_in,
                "tidal_device_auth_started"
            );
            self.pending_device_auth = Some(device_auth.clone());
            self.device_auth_started = Some(Instant::now());

            let url = if device_auth.verification_uri_complete.starts_with("http") {
                device_auth.verification_uri_complete
            } else {
                format!("https://{}", device_auth.verification_uri_complete)
            };
            return Ok(AuthStatus {
                authenticated: false,
                verification_url: Some(url),
                user_code: Some(device_auth.user_code),
                ..Default::default()
            });
        }

        // --- Poll pending Device Code flow ---
        if self.pending_device_auth.is_some() {
            info!(has_pending = true, device_code = ?self.pending_device_auth.as_ref().map(|p| &p.device_code), "tidal_auth_poll");

            if let Some(ref pending) = self.pending_device_auth.clone() {
                // Check if the device code has expired (Tidal typically gives 300s)
                let max_lifetime = Duration::from_secs(pending.expires_in.max(300));
                if let Some(started) = self.device_auth_started {
                    if started.elapsed() > max_lifetime {
                        warn!(
                            elapsed_secs = started.elapsed().as_secs(),
                            expires_in = pending.expires_in,
                            "tidal_device_code_expired — clearing pending auth"
                        );
                        self.pending_device_auth = None;
                        self.device_auth_started = None;
                        return Ok(AuthStatus {
                            authenticated: false,
                            ..Default::default()
                        });
                    }
                }

                let resp = self
                    .client
                    .post(format!("{AUTH_BASE}/token"))
                    .form(&[
                        ("client_id", CLIENT_ID),
                        ("client_secret", CLIENT_SECRET),
                        ("device_code", pending.device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("scope", "r_usr w_usr w_sub"),
                    ])
                    .send()
                    .await
                    .map_err(|e| format!("token: {e}"))?;

                let status_code = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();

                if status_code != 200 {
                    info!(status = status_code, body = %body, "tidal_token_exchange");
                    // If Tidal says the device code is expired or invalid, clear it
                    // so the user can start a fresh device auth flow.
                    if body.contains("expired") || body.contains("invalid_grant") {
                        warn!("tidal_device_code_rejected — clearing pending auth");
                        self.pending_device_auth = None;
                        self.device_auth_started = None;
                    }
                    return Ok(AuthStatus {
                        authenticated: false,
                        ..Default::default()
                    });
                }

                info!(body = %body, "tidal_token_exchange_success");

                let token: TokenResponse = serde_json::from_str(&body).map_err(|e| {
                    warn!(error = %e, body = %body, "tidal_token_parse_failed");
                    format!("token parse: {e}")
                })?;
                let access_token_clone = token.access_token.clone();
                {
                    let mut ts = self.tokens.lock().await;
                    ts.access_token = Some(token.access_token);
                    ts.refresh_token = token.refresh_token;
                    ts.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
                }
                // user_id: use the response field if present, otherwise extract from JWT
                self.user_id = token
                    .user_id
                    .or_else(|| Self::extract_uid_from_jwt(&access_token_clone));
                self.pending_device_auth = None;
                self.device_auth_started = None;
                self.country_code = "FR".into();

                info!(user_id = ?self.user_id, "tidal_authenticated");

                // Fetch username and subscription info now that we're authenticated
                self.refresh_user_info().await;
            }

            return Ok(self.auth_status().await);
        }

        // --- Default: start PKCE flow (primary) ---
        let verifier = Self::generate_code_verifier();
        let challenge = Self::generate_code_challenge(&verifier);
        let state = Self::generate_state();
        let authorize_url = Self::build_authorize_url(&challenge, &state);

        info!(url = %authorize_url, "tidal_pkce_auth_started");

        self.pending_pkce = Some(PkceState {
            code_verifier: verifier,
            state: state.clone(),
            started_at: Instant::now(),
        });

        Ok(AuthStatus {
            authenticated: false,
            verification_url: Some(authorize_url),
            ..Default::default()
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        let ts = self.tokens.lock().await;
        // Consider the token expired if we know the expiry and it's in the past.
        // Also check if we have a refresh_token — if not, a dead access_token
        // means we can't recover and must report unauthenticated.
        let token_expired = ts
            .token_expires
            .map(|t| Instant::now() > t)
            .unwrap_or(ts.refresh_token.is_none());
        let effectively_authenticated =
            ts.access_token.is_some() && (!token_expired || ts.refresh_token.is_some());
        AuthStatus {
            authenticated: effectively_authenticated,
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            expires_at: ts.token_expires.and_then(|t| {
                t.checked_duration_since(Instant::now())
                    .map(|d| format!("{}s", d.as_secs()))
            }),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        {
            let mut ts = self.tokens.lock().await;
            ts.access_token = None;
            ts.refresh_token = None;
        }
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let data = self
            .api_get(&format!(
                "/search?query={}&limit={limit}&types=TRACKS,ALBUMS,ARTISTS",
                urlencoding::encode(query)
            ))
            .await?;

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        let artists = data["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();

        Ok(SearchResults {
            tracks,
            albums,
            artists,
            playlists: vec![],
        })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        let data = self.api_get(&format!("/tracks/{track_id}")).await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        {
            let cache = self.url_cache.lock().await;
            if let Some(cached) = cache.get(track_id) {
                // A cached DASH `file://` entry becomes stale once the temp file
                // is consumed and deleted after playback. Serving it again makes
                // the transcode open a missing/empty file (os error 2) and the
                // track never plays. Only reuse a file:// entry if the file still
                // exists and is non-empty; otherwise fall through to re-download.
                let stale_dash = cached
                    .url
                    .strip_prefix("file://")
                    .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == 0)
                    .unwrap_or(false);
                if !stale_dash {
                    return Ok(StreamUrl {
                        url: cached.url.clone(),
                        mime_type: cached.mime_type.clone(),
                        quality: StreamQuality {
                            codec: cached.codec.clone(),
                            sample_rate: cached.sample_rate,
                            bit_depth: cached.bit_depth,
                            bitrate: cached.bitrate,
                            channels: 2,
                        },
                        expires_at: None,
                    });
                }
                tracing::info!(
                    track_id,
                    path = %cached.url,
                    "tidal_dash_cache_stale_redownloading"
                );
            }
        }

        let requested_quality = quality.unwrap_or(self.quality.as_str());

        // Quality fallback cascade: HI_RES_LOSSLESS → HI_RES → LOSSLESS → HIGH
        // Try the highest quality first, fall back only on API errors or
        // genuinely unsupported quality levels (e.g. subscription too low).
        // DASH manifests are now parsed to extract the direct stream URL —
        // Tidal legitimately uses DASH for Hi-Res FLAC delivery.
        let fallback_chain: &[&str] = match requested_quality {
            "HI_RES_LOSSLESS" => &["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"],
            "HI_RES" => &["HI_RES", "LOSSLESS", "HIGH"],
            "LOSSLESS" => &["LOSSLESS", "HIGH"],
            _ => &[requested_quality, "HIGH"],
        };

        let mut data = None;
        let mut downgraded_fallback: Option<serde_json::Value> = None;
        for &q in fallback_chain {
            match self.fetch_playback_info(track_id, q).await {
                Ok(d) => {
                    let has_manifest = d["manifest"].as_str().is_some();
                    if !has_manifest {
                        let manifest_mime = d["manifestMimeType"].as_str().unwrap_or("");
                        info!(
                            track_id,
                            quality = q,
                            manifest_mime,
                            has_manifest,
                            "tidal_no_manifest_trying_next"
                        );
                        continue;
                    }
                    // Check if Tidal actually returned the requested quality or
                    // downgraded silently (e.g. HI_RES_LOSSLESS → HIGH AAC).
                    let returned_quality = d["audioQuality"].as_str().unwrap_or("UNKNOWN");
                    let returned_codec = d["codec"].as_str().unwrap_or("UNKNOWN");
                    let manifest_mime = d["manifestMimeType"].as_str().unwrap_or("");
                    let bit_depth = d["bitDepth"].as_u64();
                    let sample_rate = d["sampleRate"].as_u64();

                    if returned_quality != q {
                        warn!(
                            track_id,
                            requested = q,
                            returned = returned_quality,
                            returned_codec,
                            manifest_mime,
                            ?bit_depth,
                            ?sample_rate,
                            subscription = ?self.subscription,
                            "tidal_quality_mismatch"
                        );
                    } else {
                        debug!(
                            track_id,
                            requested = q,
                            returned = returned_quality,
                            "tidal_playback_info_ok"
                        );
                    }

                    if returned_quality == q {
                        data = Some(d);
                        break;
                    }

                    // Tidal downgraded our quality. The quality hierarchy is:
                    // HI_RES_LOSSLESS > HI_RES > LOSSLESS > HIGH
                    // Accept if the returned quality is at least lossless,
                    // otherwise keep looking.
                    let returned_rank = quality_rank(returned_quality);
                    let requested_rank = quality_rank(q);

                    if returned_rank >= requested_rank {
                        // Tidal returned same or higher quality than requested — use it
                        data = Some(d);
                        break;
                    }

                    // Tidal downgraded. If returned quality is still lossless+,
                    // save it as a candidate but keep trying.
                    if returned_rank >= 2 {
                        // LOSSLESS or better — acceptable fallback
                        info!(
                            track_id,
                            requested = q,
                            returned = returned_quality,
                            "tidal_quality_downgraded_but_lossless"
                        );
                        if downgraded_fallback.is_none() {
                            downgraded_fallback = Some(d);
                        }
                    } else {
                        // Got HIGH/LOW (lossy) — save only if nothing better found
                        warn!(
                            track_id,
                            requested = q,
                            returned = returned_quality,
                            subscription = ?self.subscription,
                            "tidal_quality_downgraded_to_lossy"
                        );
                        if downgraded_fallback.is_none() {
                            downgraded_fallback = Some(d);
                        }
                    }
                }
                Err(e) => {
                    info!(
                        track_id,
                        quality = q,
                        error = %e,
                        "tidal_quality_fetch_failed_trying_next"
                    );
                }
            }
        }

        // Use the best available data: exact match first, then best downgraded fallback
        if data.is_none() {
            if let Some(fb) = downgraded_fallback {
                let fb_quality = fb["audioQuality"].as_str().unwrap_or("UNKNOWN");
                warn!(
                    track_id,
                    requested = requested_quality,
                    actual = fb_quality,
                    subscription = ?self.subscription,
                    "tidal_using_downgraded_quality_no_better_available"
                );
                data = Some(fb);
            }
        }

        let data = data.ok_or_else(|| {
            format!("tidal: no usable stream found for track {track_id} (all qualities failed)")
        })?;

        let manifest = data["manifest"].as_str().ok_or("no manifest")?;
        let manifest_mime = data["manifestMimeType"].as_str().unwrap_or("");
        let decoded =
            String::from_utf8(base64_decode(manifest).map_err(|e| format!("base64: {e}"))?)
                .map_err(|e| format!("utf8: {e}"))?;

        // Parse the manifest to get URL + optional codec/mime from the manifest itself
        let (url, dash_codec, dash_mime) = if manifest_mime == "application/dash+xml" {
            // Log raw manifest content for debugging Hi-Res issues
            debug!(
                track_id,
                manifest_len = decoded.len(),
                manifest_content = &decoded[..decoded.len().min(500)],
                "tidal_dash_manifest_raw"
            );
            // DASH manifest (MPD XML) — parse to extract URL, codec, and MIME type
            let dash = parse_dash_manifest(&decoded).ok_or_else(|| {
                warn!(
                    track_id,
                    manifest_len = decoded.len(),
                    manifest_content = &decoded[..decoded.len().min(500)],
                    "tidal_dash_parse_failed — full manifest logged above"
                );
                format!("tidal: could not parse DASH manifest for track {track_id}")
            })?;

            // DASH multi-segment (fMP4 container with SegmentTemplate + SegmentTimeline):
            // Download all segments, concatenate into a single fMP4 file, and return
            // a file:// URL so the orchestrator can read directly from disk.
            if let Some(ref seg_info) = dash.segments {
                info!(
                    track_id,
                    segment_count = seg_info.segment_count,
                    start_number = seg_info.start_number,
                    dash_codec = dash.codec.as_deref().unwrap_or("none"),
                    dash_mime = dash.mime_type.as_deref().unwrap_or("none"),
                    "tidal_dash_multi_segment_detected"
                );
                let tmp_path = self.download_dash_segments(seg_info, track_id).await?;
                let file_url = format!("file://{tmp_path}");
                (file_url, dash.codec, dash.mime_type)
            } else {
                // Single-segment DASH (BaseURL or single SegmentTemplate)
                let url_len = dash.url.len();
                if url_len < 30 {
                    warn!(
                        track_id,
                        url = dash.url.as_str(),
                        url_len,
                        "tidal_dash_suspicious_short_url"
                    );
                }
                info!(
                    track_id,
                    url = dash.url.as_str(),
                    url_len,
                    dash_codec = dash.codec.as_deref().unwrap_or("none"),
                    dash_mime = dash.mime_type.as_deref().unwrap_or("none"),
                    "tidal_dash_manifest_parsed"
                );
                (dash.url, dash.codec, dash.mime_type)
            }
        } else if let Ok(manifest_json) = serde_json::from_str::<serde_json::Value>(&decoded) {
            // BTS manifest (JSON with urls array)
            let url = manifest_json["urls"]
                .as_array()
                .and_then(|urls| urls.first())
                .and_then(|u| u.as_str())
                .ok_or("no url in manifest")?
                .to_string();
            (url, None, None)
        } else {
            (decoded, None, None)
        };

        let audio_quality = data["audioQuality"].as_str().unwrap_or("LOSSLESS");
        let (sample_rate, bit_depth) = Self::parse_quality_metadata(&data, audio_quality);

        // Determine codec: prefer DASH manifest codec, then fall back to audio quality level.
        // DASH codecs: "flac" → FLAC, "mp4a.40.2" → AAC, "mp4a.40.5" → AAC HE
        let codec = if let Some(ref dc) = dash_codec {
            let dc_lower = dc.to_lowercase();
            if dc_lower == "flac" || dc_lower.starts_with("fla") {
                "FLAC"
            } else if dc_lower.starts_with("mp4a") || dc_lower.starts_with("aac") {
                "AAC"
            } else if dc_lower.starts_with("mqa") {
                "FLAC" // MQA is delivered as FLAC container
            } else {
                // Unknown DASH codec — fall back to quality-based detection
                match audio_quality {
                    "HI_RES_LOSSLESS" | "HI_RES" | "LOSSLESS" => "FLAC",
                    _ => "AAC",
                }
            }
        } else {
            // No DASH codec info — determine from audio quality level:
            // HI_RES_LOSSLESS = FLAC Hi-Res, HI_RES = MQA/FLAC, LOSSLESS = FLAC CD
            // Only HIGH and below are AAC
            match audio_quality {
                "HI_RES_LOSSLESS" | "HI_RES" | "LOSSLESS" => "FLAC",
                _ => "AAC",
            }
        };

        // Determine MIME type: prefer DASH manifest, then derive from codec.
        // For DASH multi-segment downloads (file:// URLs), the file on disk is
        // an fMP4 container — report audio/mp4 so symphonia uses IsoMp4Reader.
        let is_dash_file = url.starts_with("file://");
        let mime_type = if is_dash_file {
            // fMP4 container on disk — always audio/mp4 regardless of inner codec
            "audio/mp4"
        } else if codec == "FLAC" {
            // DASH might report "audio/flac" or "audio/mp4" for FLAC-in-fMP4 container.
            // For playback, report what the actual codec is.
            if let Some(ref dm) = dash_mime {
                if dm == "audio/flac" || dm == "audio/mp4" {
                    dm.as_str()
                } else {
                    "audio/flac"
                }
            } else {
                "audio/flac"
            }
        } else if let Some(ref dm) = dash_mime {
            dm.as_str()
        } else if !manifest_mime.is_empty() && manifest_mime != "application/dash+xml" {
            manifest_mime
        } else {
            "audio/mp4"
        };

        let bitrate = if codec == "FLAC" {
            data["bitDepth"]
                .as_u64()
                .map(|_| sample_rate * (bit_depth as u32) * 2)
        } else {
            Some(320_000) // AAC HIGH = 320kbps
        };

        // Cache all URLs including file:// from DASH — prevents double-download
        // when gapless pre-buffers the same track. The orchestrator should NOT
        // delete the temp file until the track finishes playing.
        {
            let mut cache = self.url_cache.lock().await;
            cache.set(
                track_id.to_string(),
                CachedUrl {
                    url: url.clone(),
                    mime_type: mime_type.to_string(),
                    codec: codec.into(),
                    sample_rate,
                    bit_depth,
                    bitrate,
                    created: Instant::now(),
                },
            );
        }

        // Log the final download URL — essential for diagnosing Hi-Res issues
        // where the URL might point to the manifest itself instead of audio data.
        debug!(
            track_id,
            final_url = url.as_str(),
            url_len = url.len(),
            codec,
            mime_type,
            "tidal_final_download_url"
        );

        if codec == "AAC" {
            warn!(
                track_id,
                requested = requested_quality,
                returned = audio_quality,
                codec,
                sample_rate,
                bit_depth,
                subscription = ?self.subscription,
                "tidal_stream_url_lossy — Tidal returned AAC instead of FLAC"
            );
        } else {
            info!(
                track_id,
                requested = requested_quality,
                returned = audio_quality,
                codec,
                sample_rate,
                bit_depth,
                "tidal_stream_url"
            );
        }

        Ok(StreamUrl {
            url,
            mime_type: mime_type.to_string(),
            quality: StreamQuality {
                codec: codec.into(),
                sample_rate,
                bit_depth,
                bitrate,
                channels: 2,
            },
            expires_at: None,
        })
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        let data = self.api_get(&format!("/albums/{album_id}")).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(&format!("/albums/{album_id}/tracks?limit=100"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        let data = self.api_get(&format!("/artists/{artist_id}")).await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get(&format!("/artists/{artist_id}/albums?limit=50"))
            .await?;
        let albums = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(&format!("/artists/{artist_id}/toptracks?limit=20"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        let data = self.api_get(&format!("/playlists/{playlist_id}")).await?;
        Ok(Self::map_playlist(&data))
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(&format!("/playlists/{playlist_id}/tracks?limit=100"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_genres(&self, _parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        let data = self.api_get("/genres").await?;
        let genres = data
            .as_array()
            .map(|items| items.iter().map(Self::map_genre).collect())
            .unwrap_or_default();
        Ok(genres)
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get(&format!(
                "/genres/{}/albums?limit={limit}",
                urlencoding::encode(genre_id)
            ))
            .await?;
        let albums = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        if let Some(ref cache) = self.featured_cache
            && cache.fetched_at.elapsed() < Duration::from_secs(300)
        {
            return Ok(cache.sections.iter().map(|(s, _)| s.clone()).collect());
        }
        let data = self.api_get("/pages/home").await?;
        let mut sections = Vec::new();
        if let Some(rows) = data["rows"].as_array() {
            for (i, row) in rows.iter().enumerate() {
                let title = row["modules"]
                    .as_array()
                    .and_then(|m| m.first())
                    .and_then(|m| m["title"].as_str())
                    .unwrap_or("");
                if title.is_empty() {
                    continue;
                }
                let albums: Vec<StreamAlbum> = row["modules"]
                    .as_array()
                    .and_then(|m| m.first())
                    .and_then(|m| m["pagedList"]["items"].as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| {
                                item["type"].as_str() == Some("ALBUM")
                                    || item["id"].as_u64().is_some()
                            })
                            .map(Self::map_album)
                            .collect()
                    })
                    .unwrap_or_default();
                if albums.is_empty() {
                    continue;
                }
                sections.push((
                    FeaturedSection {
                        id: format!("home-{i}"),
                        name: title.into(),
                    },
                    albums,
                ));
            }
        }
        let result = sections.iter().map(|(s, _)| s.clone()).collect();
        // Can't cache here since &self is immutable, will use the route-level cache
        Ok(result)
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        if let Some(ref cache) = self.featured_cache
            && cache.fetched_at.elapsed() < Duration::from_secs(300)
            && let Some((_, albums)) = cache.sections.iter().find(|(s, _)| s.id == section_id)
        {
            return Ok(albums.clone());
        }
        let sections = self.get_featured_sections().await?;
        let _ = sections;
        Ok(vec![])
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self
            .api_get(&format!("/users/{user_id}/favorites/tracks?limit=100"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("item").map(Self::map_track))
                    .collect()
            })
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let user_id = self.user_id.ok_or("no user_id")?;
        let (path_type, form_key) = match fav_type {
            "tracks" => ("tracks", "trackIds"),
            "albums" => ("albums", "albumIds"),
            "artists" => ("artists", "artistIds"),
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_post_form(
            &format!("/users/{user_id}/favorites/{path_type}"),
            &[(form_key, item_id)],
        )
        .await?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let user_id = self.user_id.ok_or("no user_id")?;
        let path_type = match fav_type {
            "tracks" => "tracks",
            "albums" => "albums",
            "artists" => "artists",
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_delete(&format!("/users/{user_id}/favorites/{path_type}/{item_id}"))
            .await?;
        Ok(())
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let mut all = Vec::new();
        let mut offset = 0u32;
        let page_size = 50u32;
        loop {
            let data = self
                .api_get(&format!(
                    "/users/{user_id}/playlists?limit={page_size}&offset={offset}"
                ))
                .await?;
            let items = data["items"]
                .as_array()
                .map(|items| items.iter().map(Self::map_playlist).collect::<Vec<_>>())
                .unwrap_or_default();
            let count = items.len();
            all.extend(items);
            let total = data["totalNumberOfItems"].as_u64().unwrap_or(0) as usize;
            offset += page_size;
            if count == 0 || all.len() >= total {
                break;
            }
        }
        Ok(all)
    }

    async fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<String, TuneError> {
        let user_id = self
            .user_id
            .ok_or("tidal: not authenticated (no user_id)")?;
        let desc = description.unwrap_or("Created by Tune");
        let resp = self
            .api_post_form(
                &format!("/users/{user_id}/playlists"),
                &[("title", name), ("description", desc)],
            )
            .await?;
        resp["uuid"]
            .as_str()
            .or_else(|| resp["id"].as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "tidal: no playlist id in response".into())
    }

    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let mut added = 0;
        for chunk in track_ids.chunks(100) {
            let ids_csv = chunk.join(",");
            self.api_post_form(
                &format!("/playlists/{playlist_id}/items"),
                &[("trackIds", &ids_csv)],
            )
            .await?;
            added += chunk.len();
        }
        Ok(added)
    }

    fn supports_write(&self) -> bool {
        self.user_id.is_some()
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        // Use stored user_id instead of /users/me
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self
            .api_get(&format!("/users/{user_id}/favorites/albums?limit=100"))
            .await?;
        let albums = data["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("item").map(Self::map_album))
                    .collect()
            })
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        // Use stored user_id instead of /users/me
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self
            .api_get(&format!("/users/{user_id}/favorites/artists?limit=100"))
            .await?;
        let artists = data["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("item").map(Self::map_artist))
                    .collect()
            })
            .unwrap_or_default();
        Ok(artists)
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        let data = self.api_get("/featured/playlists?limit=50").await?;
        let playlists = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_playlist).collect())
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self.api_get("/featured/new/albums?limit=50").await?;
        let albums = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn post_restore(&mut self) {
        self.refresh_user_info().await;
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        let needs_refresh = {
            let ts = self.tokens.lock().await;
            ts.token_expires
                .map(|exp| {
                    exp.checked_duration_since(Instant::now())
                        .map(|d| d.as_secs() < 300)
                        .unwrap_or(true)
                })
                .unwrap_or_else(|| {
                    // No expiry tracked (e.g. restored from DB) — proactively refresh
                    // if we have both an access_token and a refresh_token
                    ts.access_token.is_some() && ts.refresh_token.is_some()
                })
        };

        if !needs_refresh {
            return Ok(false);
        }

        // Delegate to do_refresh_token which handles the Mutex internally
        Ok(self.do_refresh_token().await?)
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        let mut obj = serde_json::json!({});
        // Use try_lock since save_tokens is sync — if the mutex is held, skip saving
        if let Ok(ts) = self.tokens.try_lock()
            && let Some(ref token) = ts.access_token
        {
            obj["access_token"] = serde_json::json!(token);
            obj["refresh_token"] = serde_json::json!(ts.refresh_token);
            obj["username"] = serde_json::json!(self.username);
            obj["country_code"] = serde_json::json!(self.country_code);
            obj["user_id"] = serde_json::json!(self.user_id);
        }
        if let Some(ref pending) = self.pending_device_auth {
            obj["pending_device_code"] = serde_json::json!(pending.device_code);
            obj["pending_user_code"] = serde_json::json!(pending.user_code);
            obj["pending_uri"] = serde_json::json!(pending.verification_uri_complete);
        }
        if obj.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            None
        } else {
            Some(obj)
        }
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        let mut restored = false;
        if let Some(at) = tokens["access_token"].as_str() {
            let refresh_token = tokens["refresh_token"].as_str().map(String::from);
            // Use try_lock since restore_tokens is sync
            if let Ok(mut ts) = self.tokens.try_lock() {
                ts.access_token = Some(at.into());
                ts.refresh_token = refresh_token.clone();
                // token_expires stays None — refresh_if_needed will proactively
                // refresh on the first 5-minute tick, and api_get will refresh
                // on-demand if a 401 comes back before then
            }
            self.username = tokens["username"].as_str().map(Into::into);
            self.country_code = tokens["country_code"].as_str().unwrap_or("FR").into();
            self.user_id = tokens["user_id"].as_u64().or_else(|| {
                refresh_token
                    .as_deref()
                    .and_then(Self::extract_uid_from_jwt)
            });
            restored = true;
        }
        if let Some(dc) = tokens["pending_device_code"].as_str() {
            self.pending_device_auth = Some(DeviceAuthResponse {
                device_code: dc.into(),
                user_code: tokens["pending_user_code"].as_str().unwrap_or("").into(),
                verification_uri: "link.tidal.com".into(),
                verification_uri_complete: tokens["pending_uri"].as_str().unwrap_or("").into(),
                expires_in: 300,
                interval: 2,
            });
            // Set device_auth_started to now — the code may already be expired
            // from a previous session, but the expiration check in authenticate()
            // will clear it on the first poll attempt.
            self.device_auth_started = Some(Instant::now());
            restored = true;
        }
        restored
    }
}

/// Rank audio quality levels for comparison. Higher = better.
fn quality_rank(quality: &str) -> u8 {
    match quality {
        "HI_RES_LOSSLESS" => 4,
        "HI_RES" => 3,
        "LOSSLESS" => 2,
        "HIGH" => 1,
        _ => 0,
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;

    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        let val = table
            .iter()
            .position(|&c| c == byte)
            .ok_or("invalid base64")? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(output)
}

/// Base64url-encode (no padding) — used for PKCE code challenges.
fn base64url_encode(data: &[u8]) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            output.push(table[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        buf <<= 6 - bits;
        output.push(table[(buf & 0x3F) as usize] as char);
    }
    output
}

fn base64_decode_url(input: &str) -> Result<Vec<u8>, String> {
    let padded = match input.len() % 4 {
        2 => format!("{input}=="),
        3 => format!("{input}="),
        _ => input.to_string(),
    };
    let standard = padded.replace('-', "+").replace('_', "/");
    base64_decode(&standard)
}

/// Extract the first `<BaseURL>` content from a DASH MPD manifest.
/// Tidal Hi-Res FLAC streams are delivered via DASH; the MPD XML contains
/// one or more `<BaseURL>` elements with direct HTTPS URLs to the FLAC data.
/// Decide whether a token-endpoint error means the refresh_token is PERMANENTLY
/// invalid and all tokens should be cleared.
///
/// Only returns `true` for:
/// - 401 Unauthorized (bad credentials)
/// - 400 with body containing "invalid_grant" or "invalid_client" (token
///   revoked/expired/wrong client)
///
/// Returns `false` for transient errors like 429 (rate limit), other 400
/// sub-errors, or 5xx server errors.
fn is_refresh_permanently_invalid(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if status == 400 {
        let lower = body.to_lowercase();
        return lower.contains("invalid_grant") || lower.contains("invalid_client");
    }
    false
}

/// Parsed result from a DASH MPD manifest.
struct DashManifest {
    /// Direct stream URL (from `<BaseURL>` or constructed from `<SegmentTemplate>`)
    url: String,
    /// Codec string from `<Representation codecs="...">` (e.g. "flac", "mp4a.40.2")
    codec: Option<String>,
    /// MIME type from `<Representation mimeType="...">` or `<AdaptationSet mimeType="...">`
    mime_type: Option<String>,
    /// Multi-segment info for DASH SegmentTemplate + SegmentTimeline delivery.
    /// Present when the manifest uses segmented delivery (fMP4 container).
    segments: Option<DashSegmentInfo>,
}

/// Segment information extracted from DASH SegmentTemplate + SegmentTimeline.
/// Tidal Hi-Res FLAC uses fMP4 with multiple segments that must all be
/// downloaded and concatenated to reconstruct the full audio file.
#[derive(Debug, Clone)]
struct DashSegmentInfo {
    /// Initialization segment URL (MP4 header / ftyp+moov boxes)
    init_url: String,
    /// Media segment URL template (contains `$Number$` placeholder)
    media_template: String,
    /// Start number for segment numbering (usually 1)
    start_number: u32,
    /// Total number of media segments to download
    segment_count: u32,
}

/// Default Tidal CDN base used to resolve relative URLs in DASH manifests.
/// Tidal Hi-Res FLAC manifests sometimes use relative `<BaseURL>` paths
/// (e.g. `mediatracks/CAEaKgo/0.flac`) that need a CDN prefix.
const TIDAL_CDN_BASE: &str = "https://sp-pr-cf.audio.tidal.com/";

/// Resolve a potentially relative URL against the Tidal CDN base.
/// Returns the URL unchanged if it's already absolute (starts with `http`).
fn resolve_tidal_url(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else if url.starts_with('/') {
        // Absolute path — prepend scheme + host
        format!("https://sp-pr-cf.audio.tidal.com{url}")
    } else {
        // Relative path — prepend full CDN base
        format!("{TIDAL_CDN_BASE}{url}")
    }
}

/// Parse a DASH MPD manifest (XML) to extract stream URL, codec, and MIME type.
///
/// Tidal Hi-Res FLAC streams use DASH manifests. The MPD XML typically contains
/// a single `<Representation>` with either a `<BaseURL>` (direct HTTPS URL to
/// the FLAC data) or a `<SegmentTemplate>` (URL template for segmented delivery).
///
/// Relative BaseURL and SegmentTemplate URLs are resolved against the Tidal CDN
/// base (`sp-pr-cf.audio.tidal.com`). This is essential for Hi-Res 24-bit tracks
/// where Tidal sometimes omits the absolute URL prefix.
///
/// Uses `quick_xml` which is already a project dependency.
fn parse_dash_manifest(mpd: &str) -> Option<DashManifest> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(mpd);

    let mut base_url: Option<String> = None;
    let mut codec: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut segment_template_media: Option<String> = None;
    let mut segment_template_init: Option<String> = None;
    let mut segment_template_start_number: u32 = 1;
    let mut in_base_url = false;
    let mut in_segment_template = false;
    // SegmentTimeline: accumulate total segment count from <S> elements.
    // Each <S d="..." r="N"/> means 1 + N segments (r = repeat count, 0-based).
    let mut timeline_segment_count: u32 = 0;
    let mut has_segment_timeline = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                let tag = std::str::from_utf8(local.as_ref()).unwrap_or("");

                match tag {
                    "BaseURL" => {
                        in_base_url = true;
                    }
                    "AdaptationSet" => {
                        // Pick up mimeType from AdaptationSet if not already set
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "mimeType" && mime_type.is_none() {
                                mime_type = Some(val);
                            } else if key == "codecs" && codec.is_none() {
                                codec = Some(val);
                            }
                        }
                    }
                    "Representation" => {
                        // Representation attributes override AdaptationSet
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "codecs" => codec = Some(val),
                                "mimeType" => mime_type = Some(val),
                                _ => {}
                            }
                        }
                    }
                    "SegmentTemplate" => {
                        in_segment_template = true;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "media" => {
                                    segment_template_media = Some(val);
                                }
                                "initialization" => {
                                    segment_template_init = Some(val);
                                }
                                "startNumber" => {
                                    segment_template_start_number = val.parse().unwrap_or(1);
                                }
                                _ => {}
                            }
                        }
                    }
                    "SegmentTimeline" => {
                        has_segment_timeline = true;
                    }
                    "S" if in_segment_template || has_segment_timeline => {
                        // <S d="380928" r="52"/> means 1 + 52 = 53 segments
                        // <S t="0" d="380928"/> means 1 segment (r defaults to 0)
                        let mut repeat: u32 = 0;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "r" {
                                repeat = val.parse().unwrap_or(0);
                            }
                        }
                        // 1 for this <S> element + r repeats
                        timeline_segment_count += 1 + repeat;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_base_url {
                    let decoded = e.decode().unwrap_or_default();
                    let url = decoded.trim().to_string();
                    if !url.is_empty() {
                        base_url = Some(url);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                let tag = std::str::from_utf8(local.as_ref()).unwrap_or("");
                match tag {
                    "BaseURL" => in_base_url = false,
                    "SegmentTemplate" => in_segment_template = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // Build segment info if we have a SegmentTemplate with SegmentTimeline
    let segments = if has_segment_timeline
        && timeline_segment_count > 0
        && segment_template_media.is_some()
        && segment_template_init.is_some()
    {
        let init_url = resolve_tidal_url(segment_template_init.as_ref().unwrap());
        let media_template = resolve_tidal_url(segment_template_media.as_ref().unwrap());
        if init_url.starts_with("http") && media_template.starts_with("http") {
            Some(DashSegmentInfo {
                init_url,
                media_template,
                start_number: segment_template_start_number,
                segment_count: timeline_segment_count,
            })
        } else {
            None
        }
    } else {
        None
    };

    // Prefer BaseURL (direct stream), fall back to SegmentTemplate media URL
    // (NOT initialization — init is just the MP4 header, media is the actual audio).
    // Resolve relative URLs against the Tidal CDN base.
    let url = if let Some(ref u) = base_url {
        let resolved = resolve_tidal_url(u);
        if !resolved.starts_with("http") {
            warn!(
                raw_base_url = u.as_str(),
                resolved = resolved.as_str(),
                "tidal_dash_base_url_unresolvable"
            );
            return None;
        }
        if u != &resolved {
            info!(
                raw = u.as_str(),
                resolved = resolved.as_str(),
                "tidal_dash_relative_base_url_resolved"
            );
        }
        resolved
    } else if let Some(ref media) = segment_template_media {
        // SegmentTemplate with SegmentTimeline: use the init URL as the
        // primary URL (the segments field carries the full download info).
        // Without SegmentTimeline: expand template for single-segment access.
        if segments.is_some() {
            // Multi-segment: use init URL as the representative URL.
            // The actual download is done via DashSegmentInfo.
            resolve_tidal_url(segment_template_init.as_deref().unwrap_or(media))
        } else {
            // Single-segment fallback: replace template variables with defaults.
            let expanded = media
                .replace("$Number$", "0")
                .replace("$RepresentationID$", "1")
                .replace("$Bandwidth$", "0")
                .replace("$Time$", "0");
            let resolved = resolve_tidal_url(&expanded);
            if !resolved.starts_with("http") {
                warn!(
                    media_template = media.as_str(),
                    expanded = expanded.as_str(),
                    init_url = segment_template_init.as_deref().unwrap_or("none"),
                    "tidal_dash_segment_template_unresolvable"
                );
                return None;
            }
            info!(
                media_template = media.as_str(),
                resolved = resolved.as_str(),
                init_url = segment_template_init.as_deref().unwrap_or("none"),
                "tidal_dash_segment_template_resolved"
            );
            resolved
        }
    } else if let Some(ref init) = segment_template_init {
        // Last resort: initialization-only SegmentTemplate (no media attr).
        // This is typically just the MP4 header — it will be small, but
        // it's better to try than to fail silently.
        let resolved = resolve_tidal_url(init);
        if !resolved.starts_with("http") {
            warn!(
                init_url = init.as_str(),
                "tidal_dash_init_only_segment_template_unresolvable"
            );
            return None;
        }
        warn!(
            init_url = init.as_str(),
            resolved = resolved.as_str(),
            "tidal_dash_init_only_no_media_template — URL may be header-only"
        );
        resolved
    } else {
        return None;
    };

    Some(DashManifest {
        url,
        codec,
        mime_type,
        segments,
    })
}

/// Extract the direct stream URL from a DASH MPD manifest.
/// Convenience wrapper around `parse_dash_manifest`.
#[cfg(test)]
fn extract_dash_base_url(mpd: &str) -> Option<String> {
    parse_dash_manifest(mpd).map(|m| m.url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_track_basic() {
        let json = json!({
            "id": 123,
            "title": "So What",
            "artist": {"name": "Miles Davis"},
            "album": {"title": "Kind of Blue", "cover": "abc-def-ghi", "id": 456},
            "duration": 562,
            "trackNumber": 1,
            "volumeNumber": 1,
            "explicit": false,
            "audioQuality": "LOSSLESS",
        });
        let track = TidalService::map_track(&json);
        assert_eq!(track.id, "123");
        assert_eq!(track.title, "So What");
        assert_eq!(track.artist, "Miles Davis");
        assert_eq!(track.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(track.album_id.as_deref(), Some("456"));
        assert_eq!(track.duration_ms, 562_000);
        assert_eq!(track.track_number, Some(1));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert!(track.cover_path.is_some());
        let cover = track.cover_path.unwrap();
        assert!(cover.contains("resources.tidal.com"));
        assert!(cover.contains("abc/def/ghi"));
    }

    #[test]
    fn map_track_hires() {
        let json = json!({
            "id": 999,
            "title": "Hi-Res Track",
            "artist": {"name": "Test"},
            "album": {"title": "Album", "cover": "xx-yy", "id": 1},
            "duration": 300,
            "trackNumber": 1,
            "volumeNumber": 1,
            "explicit": true,
            "audioQuality": "HI_RES_LOSSLESS",
            "mediaMetadata": {"tags": ["HIRES_LOSSLESS"]},
        });
        let track = TidalService::map_track(&json);
        assert!(track.explicit);
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 96000);
        assert_eq!(q.bit_depth, 24);
        assert_eq!(q.codec, "FLAC");
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": 0,
            "title": null,
            "artist": {},
            "album": {},
            "duration": null,
        });
        let track = TidalService::map_track(&json);
        assert_eq!(track.id, "0");
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
        assert!(track.album.is_none());
        assert!(track.cover_path.is_none());
    }

    #[test]
    fn map_track_artists_array() {
        let json = json!({
            "id": 1,
            "title": "Test",
            "artists": [{"name": "First Artist"}, {"name": "Second"}],
            "album": {"title": "A", "id": 1},
            "duration": 100,
        });
        let track = TidalService::map_track(&json);
        assert_eq!(track.artist, "First Artist");
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": 789,
            "title": "Kind of Blue",
            "artist": {"name": "Miles Davis", "id": 42},
            "cover": "abc-def-ghi",
            "releaseDate": "1959-08-17",
            "numberOfTracks": 5,
        });
        let album = TidalService::map_album(&json);
        assert_eq!(album.id, "789");
        assert_eq!(album.title, "Kind of Blue");
        assert_eq!(album.artist, "Miles Davis");
        assert_eq!(album.artist_id.as_deref(), Some("42"));
        assert_eq!(album.year, Some(1959));
        assert_eq!(album.track_count, 5);
        assert!(album.cover_path.is_some());
    }

    #[test]
    fn map_album_missing_fields() {
        let json = json!({
            "id": 0,
            "title": null,
            "artist": {},
        });
        let album = TidalService::map_album(&json);
        assert_eq!(album.id, "0");
        assert_eq!(album.title, "");
        assert_eq!(album.artist, "");
        assert!(album.year.is_none());
        assert_eq!(album.track_count, 0);
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": 42,
            "name": "Miles Davis",
            "picture": "aa-bb-cc-dd",
        });
        let artist = TidalService::map_artist(&json);
        assert_eq!(artist.id, "42");
        assert_eq!(artist.name, "Miles Davis");
        assert!(artist.image_path.is_some());
        let img = artist.image_path.unwrap();
        assert!(img.contains("480x480"));
    }

    #[test]
    fn map_artist_no_picture() {
        let json = json!({
            "id": 1,
            "name": "Unknown",
        });
        let artist = TidalService::map_artist(&json);
        assert!(artist.image_path.is_none());
    }

    #[test]
    fn map_genre_basic() {
        let json = json!({
            "path": "jazz",
            "name": "Jazz",
            "hasSubgenres": true,
            "image": "http://example.com/jazz.jpg",
        });
        let genre = TidalService::map_genre(&json);
        assert_eq!(genre.id, "jazz");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://example.com/jazz.jpg")
        );
    }

    #[test]
    fn map_genre_no_subgenres() {
        let json = json!({
            "path": "rock",
            "name": "Rock",
            "hasSubgenres": false,
        });
        let genre = TidalService::map_genre(&json);
        assert!(!genre.has_children);
    }

    #[test]
    fn parse_quality_metadata_from_response() {
        let data = json!({"bitDepth": 24, "sampleRate": 96000});
        let (sr, bd) = TidalService::parse_quality_metadata(&data, "LOSSLESS");
        assert_eq!(sr, 96000);
        assert_eq!(bd, 24);
    }

    #[test]
    fn parse_quality_metadata_fallback_hires() {
        let data = json!({});
        let (sr, bd) = TidalService::parse_quality_metadata(&data, "HI_RES_LOSSLESS");
        assert_eq!(sr, 96000);
        assert_eq!(bd, 24);
    }

    #[test]
    fn parse_quality_metadata_fallback_lossless() {
        let data = json!({});
        let (sr, bd) = TidalService::parse_quality_metadata(&data, "LOSSLESS");
        assert_eq!(sr, 44100);
        assert_eq!(bd, 16);
    }

    #[test]
    fn base64_decode_roundtrip() {
        let result = base64_decode("SGVsbG8gV29ybGQ=").unwrap();
        assert_eq!(String::from_utf8(result).unwrap(), "Hello World");
    }

    #[test]
    fn base64_decode_url_variant() {
        // "Hello" in base64url without padding
        let result = base64_decode_url("SGVsbG8").unwrap();
        assert_eq!(String::from_utf8(result).unwrap(), "Hello");
    }

    #[test]
    fn extract_dash_base_url_valid() {
        let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="audio/flac">
      <Representation>
        <BaseURL>https://sp-pr-fa.audio.tidal.com/mediatracks/abc123/0.flac</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let url = extract_dash_base_url(mpd);
        assert_eq!(
            url.as_deref(),
            Some("https://sp-pr-fa.audio.tidal.com/mediatracks/abc123/0.flac")
        );
    }

    #[test]
    fn extract_dash_base_url_missing() {
        let mpd = r#"<?xml version="1.0"?><MPD><Period></Period></MPD>"#;
        assert!(extract_dash_base_url(mpd).is_none());
    }

    #[test]
    fn extract_dash_base_url_relative_resolved() {
        let mpd = "<MPD><BaseURL>mediatracks/abc123/0.flac</BaseURL></MPD>";
        let url = extract_dash_base_url(mpd);
        assert_eq!(
            url.as_deref(),
            Some("https://sp-pr-cf.audio.tidal.com/mediatracks/abc123/0.flac")
        );
    }

    #[test]
    fn parse_dash_manifest_flac_hires() {
        let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="audio/flac" codecs="flac">
      <Representation id="1" bandwidth="2304000" codecs="flac" mimeType="audio/flac">
        <BaseURL>https://sp-pr-fa.audio.tidal.com/mediatracks/CAEaKgo/0.flac</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(
            result.url,
            "https://sp-pr-fa.audio.tidal.com/mediatracks/CAEaKgo/0.flac"
        );
        assert_eq!(result.codec.as_deref(), Some("flac"));
        assert_eq!(result.mime_type.as_deref(), Some("audio/flac"));
    }

    #[test]
    fn parse_dash_manifest_aac() {
        let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="audio/mp4">
      <Representation codecs="mp4a.40.2" mimeType="audio/mp4" bandwidth="320000">
        <BaseURL>https://sp-pr-cf.audio.tidal.com/mediatracks/xyz/0.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(
            result.url,
            "https://sp-pr-cf.audio.tidal.com/mediatracks/xyz/0.mp4"
        );
        assert_eq!(result.codec.as_deref(), Some("mp4a.40.2"));
        assert_eq!(result.mime_type.as_deref(), Some("audio/mp4"));
    }

    #[test]
    fn parse_dash_manifest_codec_from_adaptation_set() {
        // When Representation has no codecs attr, fall back to AdaptationSet
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/flac" codecs="flac">
          <Representation><BaseURL>https://example.com/track.flac</BaseURL></Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(result.codec.as_deref(), Some("flac"));
        assert_eq!(result.mime_type.as_deref(), Some("audio/flac"));
    }

    #[test]
    fn parse_dash_manifest_representation_overrides_adaptation_set() {
        // Representation codecs should override AdaptationSet codecs
        let mpd = r#"<MPD><Period>
          <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2">
            <Representation codecs="flac" mimeType="audio/flac">
              <BaseURL>https://example.com/track.flac</BaseURL>
            </Representation>
          </AdaptationSet>
        </Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(result.codec.as_deref(), Some("flac"));
        assert_eq!(result.mime_type.as_deref(), Some("audio/flac"));
    }

    #[test]
    fn parse_dash_manifest_no_base_url() {
        let mpd = r#"<MPD><Period><AdaptationSet><Representation codecs="flac">
        </Representation></AdaptationSet></Period></MPD>"#;
        assert!(parse_dash_manifest(mpd).is_none());
    }

    #[test]
    fn parse_dash_manifest_relative_base_url_resolved() {
        let mpd = r#"<MPD><Period><AdaptationSet><Representation>
          <BaseURL>mediatracks/CAEaKgo/0.flac</BaseURL>
        </Representation></AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(
            result.url,
            "https://sp-pr-cf.audio.tidal.com/mediatracks/CAEaKgo/0.flac"
        );
    }

    #[test]
    fn parse_dash_manifest_absolute_path_base_url() {
        let mpd = r#"<MPD><Period><AdaptationSet><Representation>
          <BaseURL>/mediatracks/xyz/0.flac</BaseURL>
        </Representation></AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(
            result.url,
            "https://sp-pr-cf.audio.tidal.com/mediatracks/xyz/0.flac"
        );
    }

    #[test]
    fn parse_dash_manifest_segment_template_media() {
        // SegmentTemplate with media attribute should be used (not initialization)
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/flac" codecs="flac">
          <Representation>
            <SegmentTemplate initialization="https://cdn.tidal.com/init.mp4" media="https://cdn.tidal.com/seg-$Number$.flac"/>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(result.url, "https://cdn.tidal.com/seg-0.flac");
    }

    #[test]
    fn parse_dash_manifest_segment_template_relative() {
        // Relative SegmentTemplate media URL should be resolved against Tidal CDN
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/flac">
          <Representation>
            <SegmentTemplate media="mediatracks/abc/seg-$Number$.flac" initialization="mediatracks/abc/init.mp4"/>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert_eq!(
            result.url,
            "https://sp-pr-cf.audio.tidal.com/mediatracks/abc/seg-0.flac"
        );
    }

    #[test]
    fn parse_dash_manifest_segment_timeline_basic() {
        // Real-world Tidal Hi-Res FLAC DASH manifest structure:
        // SegmentTemplate with SegmentTimeline containing <S> elements.
        // <S d="380928"/> = 1 segment, <S d="380928" r="52"/> = 53 segments.
        let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD type="static" mediaPresentationDuration="PT3M33.233S">
  <Period id="0">
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <Representation codecs="flac" bandwidth="3000000" audioSamplingRate="96000">
        <SegmentTemplate
          timescale="96000"
          media="https://sp-ad-fa.audio.tidal.com/mediatracks/abc123/$Number$.mp4?token=xyz"
          initialization="https://sp-ad-fa.audio.tidal.com/mediatracks/abc123/0.mp4?token=xyz"
          startNumber="1">
          <SegmentTimeline>
            <S t="0" d="380928"/>
            <S d="380928" r="52"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        // Should detect multi-segment and populate segments field
        assert!(result.segments.is_some(), "segments should be Some");
        let seg = result.segments.unwrap();
        assert_eq!(seg.start_number, 1);
        // 1 (first <S>) + 1 + 52 (second <S> with r=52) = 54 total segments
        assert_eq!(seg.segment_count, 54);
        assert_eq!(
            seg.init_url,
            "https://sp-ad-fa.audio.tidal.com/mediatracks/abc123/0.mp4?token=xyz"
        );
        assert!(seg.media_template.contains("$Number$"));
        assert_eq!(result.codec.as_deref(), Some("flac"));
        assert_eq!(result.mime_type.as_deref(), Some("audio/mp4"));
    }

    #[test]
    fn parse_dash_manifest_segment_timeline_single_s_with_repeat() {
        // Single <S> with r=99 means 100 segments total
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
          <Representation codecs="flac">
            <SegmentTemplate
              media="https://cdn.tidal.com/track/$Number$.mp4"
              initialization="https://cdn.tidal.com/track/0.mp4"
              startNumber="1">
              <SegmentTimeline>
                <S d="480000" r="99"/>
              </SegmentTimeline>
            </SegmentTemplate>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        let seg = result.segments.unwrap();
        assert_eq!(seg.segment_count, 100);
        assert_eq!(seg.start_number, 1);
    }

    #[test]
    fn parse_dash_manifest_no_segment_timeline_no_segments() {
        // SegmentTemplate WITHOUT SegmentTimeline should NOT populate segments
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/flac" codecs="flac">
          <Representation>
            <SegmentTemplate initialization="https://cdn.tidal.com/init.mp4" media="https://cdn.tidal.com/seg-$Number$.flac"/>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert!(
            result.segments.is_none(),
            "segments should be None without SegmentTimeline"
        );
        // URL should still be expanded from media template
        assert_eq!(result.url, "https://cdn.tidal.com/seg-0.flac");
    }

    #[test]
    fn parse_dash_manifest_base_url_no_segments() {
        // BaseURL manifests should never have segments (they're single-file)
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/flac">
          <Representation codecs="flac">
            <BaseURL>https://cdn.tidal.com/track.flac</BaseURL>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        assert!(result.segments.is_none());
        assert_eq!(result.url, "https://cdn.tidal.com/track.flac");
    }

    #[test]
    fn parse_dash_manifest_segment_timeline_url_is_init() {
        // When multi-segment is detected, the returned url should be the init URL
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
          <Representation codecs="flac">
            <SegmentTemplate
              media="https://cdn.tidal.com/$Number$.mp4"
              initialization="https://cdn.tidal.com/0.mp4"
              startNumber="1">
              <SegmentTimeline>
                <S d="380928" r="5"/>
              </SegmentTimeline>
            </SegmentTemplate>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let result = parse_dash_manifest(mpd).unwrap();
        // When segments are present, url should be the init URL
        assert_eq!(result.url, "https://cdn.tidal.com/0.mp4");
        let seg = result.segments.unwrap();
        assert_eq!(seg.segment_count, 6); // 1 + r=5
        assert_eq!(seg.media_template, "https://cdn.tidal.com/$Number$.mp4");
    }

    #[test]
    fn parse_dash_manifest_empty() {
        assert!(parse_dash_manifest("").is_none());
        assert!(parse_dash_manifest("<MPD></MPD>").is_none());
    }

    #[test]
    fn parse_quality_metadata_fallback_hi_res() {
        let data = json!({});
        let (sr, bd) = TidalService::parse_quality_metadata(&data, "HI_RES");
        assert_eq!(sr, 48000);
        assert_eq!(bd, 24);
    }

    #[test]
    fn map_playlist_basic() {
        let json = json!({
            "uuid": "abc-123",
            "title": "Jazz Essentials",
            "description": "The best jazz tracks",
            "squareImage": "aa-bb-cc-dd",
            "numberOfTracks": 42,
            "creator": {"name": "TIDAL"},
        });
        let playlist = TidalService::map_playlist(&json);
        assert_eq!(playlist.id, "abc-123");
        assert_eq!(playlist.name, "Jazz Essentials");
        assert_eq!(
            playlist.description.as_deref(),
            Some("The best jazz tracks")
        );
        assert_eq!(playlist.track_count, 42);
        assert_eq!(playlist.owner.as_deref(), Some("TIDAL"));
        assert!(playlist.cover_path.is_some());
        let cover = playlist.cover_path.unwrap();
        assert!(cover.contains("resources.tidal.com"));
        assert!(cover.contains("aa/bb/cc/dd"));
        assert!(cover.contains("640x640"));
    }

    #[test]
    fn map_playlist_image_fallback() {
        let json = json!({
            "uuid": "xyz",
            "title": "Test",
            "image": "ee-ff-gg",
            "numberOfTracks": 10,
        });
        let playlist = TidalService::map_playlist(&json);
        assert!(playlist.cover_path.is_some());
        let cover = playlist.cover_path.unwrap();
        assert!(cover.contains("ee/ff/gg"));
    }

    #[test]
    fn map_playlist_http_image() {
        let json = json!({
            "uuid": "xyz",
            "title": "Test",
            "image": "https://example.com/cover.jpg",
            "numberOfTracks": 5,
        });
        let playlist = TidalService::map_playlist(&json);
        assert_eq!(
            playlist.cover_path.as_deref(),
            Some("https://example.com/cover.jpg")
        );
    }

    #[test]
    fn map_playlist_missing_fields() {
        let json = json!({
            "uuid": null,
            "title": null,
        });
        let playlist = TidalService::map_playlist(&json);
        assert_eq!(playlist.id, "");
        assert_eq!(playlist.name, "");
        assert!(playlist.description.is_none());
        assert!(playlist.cover_path.is_none());
        assert_eq!(playlist.track_count, 0);
        assert!(playlist.owner.is_none());
    }

    #[test]
    fn tidal_service_default() {
        let svc = TidalService::new();
        assert_eq!(svc.name(), "tidal");
        assert!(svc.enabled());
        assert_eq!(svc.country_code, "US");
    }

    #[test]
    fn tidal_save_tokens_no_auth() {
        let svc = TidalService::new();
        let tokens = svc.save_tokens();
        assert!(tokens.is_none());
    }

    #[test]
    fn tidal_restore_tokens() {
        let mut svc = TidalService::new();
        let tokens = json!({
            "access_token": "test-token",
            "refresh_token": "refresh-token",
            "username": "testuser",
            "country_code": "FR",
            "user_id": 12345,
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
        assert_eq!(svc.country_code, "FR");
        assert_eq!(svc.user_id, Some(12345));
    }

    #[test]
    fn tidal_set_enabled() {
        let mut svc = TidalService::new();
        assert!(svc.enabled());
        svc.set_enabled(false);
        assert!(!svc.enabled());
        svc.set_enabled(true);
        assert!(svc.enabled());
    }

    #[test]
    fn tidal_supports_write() {
        let mut svc = TidalService::new();
        assert!(!svc.supports_write());
        svc.user_id = Some(12345);
        assert!(svc.supports_write());
    }

    #[test]
    fn refresh_permanently_invalid_401() {
        // 401 always means permanently invalid regardless of body
        assert!(is_refresh_permanently_invalid(401, ""));
        assert!(is_refresh_permanently_invalid(401, "anything"));
    }

    #[test]
    fn refresh_permanently_invalid_400_invalid_grant() {
        assert!(is_refresh_permanently_invalid(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#,
        ));
    }

    #[test]
    fn refresh_permanently_invalid_400_invalid_client() {
        assert!(is_refresh_permanently_invalid(
            400,
            r#"{"error":"invalid_client"}"#,
        ));
    }

    #[test]
    fn refresh_not_permanently_invalid_429() {
        // 429 Too Many Requests — transient, must NOT clear tokens
        assert!(!is_refresh_permanently_invalid(429, "rate limited"));
    }

    #[test]
    fn refresh_not_permanently_invalid_400_other() {
        // 400 without invalid_grant/invalid_client — should NOT clear tokens
        assert!(!is_refresh_permanently_invalid(
            400,
            r#"{"error":"server_error","error_description":"temporary failure"}"#,
        ));
    }

    #[test]
    fn refresh_not_permanently_invalid_500() {
        // 5xx server errors — transient, must NOT clear tokens
        assert!(!is_refresh_permanently_invalid(500, "internal error"));
        assert!(!is_refresh_permanently_invalid(503, "service unavailable"));
    }

    #[test]
    fn refresh_permanently_invalid_case_insensitive() {
        // Body matching should be case-insensitive
        assert!(is_refresh_permanently_invalid(
            400,
            r#"{"error":"Invalid_Grant"}"#,
        ));
        assert!(is_refresh_permanently_invalid(
            400,
            r#"{"error":"INVALID_CLIENT"}"#,
        ));
    }

    // ---- PKCE tests ----

    #[test]
    fn pkce_client_id_is_correct() {
        // Regression test: ensure we're using our own registered Tidal app
        assert_eq!(CLIENT_ID, "fX2JxdmntZWK0ixT");
    }

    #[test]
    fn pkce_code_verifier_length() {
        let verifier = TidalService::generate_code_verifier();
        assert!(
            verifier.len() >= 43 && verifier.len() <= 128,
            "code_verifier length {} not in 43..=128",
            verifier.len()
        );
    }

    #[test]
    fn pkce_code_verifier_url_safe() {
        let verifier = TidalService::generate_code_verifier();
        for c in verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == '~',
                "code_verifier contains invalid char: {c}"
            );
        }
    }

    #[test]
    fn pkce_code_verifier_unique() {
        // Two generated verifiers must be different (randomness check)
        let v1 = TidalService::generate_code_verifier();
        let v2 = TidalService::generate_code_verifier();
        assert_ne!(v1, v2, "code verifiers should be unique");
    }

    #[test]
    fn pkce_code_challenge_is_sha256_base64url() {
        // Known test vector: SHA256("foobar") = base64url encoded
        let challenge = TidalService::generate_code_challenge("foobar");
        // SHA256("foobar") = w6uP8Tcg6K2QR905Rms8iXTlksL6OD1KOWBxTK7wxPI
        // Verify it's the correct SHA256+base64url
        let mut hasher = Sha256::new();
        hasher.update(b"foobar");
        let hash = hasher.finalize();
        let expected = base64url_encode(&hash);
        assert_eq!(challenge, expected);
        // Must not contain + or / or = (those are standard base64, not base64url)
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn pkce_authorize_url_format() {
        let challenge = "test_challenge_abc123";
        let state = "test_state_xyz";
        let url = TidalService::build_authorize_url(challenge, state);

        assert!(url.starts_with("https://login.tidal.com/authorize?"));
        assert!(url.contains("client_id=fX2JxdmntZWK0ixT"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("localhost%3A8888"));
        assert!(url.contains("code_challenge=test_challenge_abc123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=test_state_xyz"));
        assert!(url.contains("scope="));
        assert!(url.contains("user.read"));
        assert!(url.contains("playback"));
    }

    #[test]
    fn pkce_redirect_uri_is_correct() {
        assert_eq!(
            REDIRECT_URI,
            "http://localhost:8888/api/v1/streaming/tidal/callback"
        );
    }

    #[test]
    fn base64url_encode_known_vector() {
        // Known: base64url("Hello") = "SGVsbG8"
        let result = base64url_encode(b"Hello");
        assert_eq!(result, "SGVsbG8");
        // Ensure no padding
        assert!(!result.contains('='));
    }

    #[test]
    fn resolve_tidal_url_absolute() {
        assert_eq!(
            resolve_tidal_url("https://sp-pr-fa.audio.tidal.com/mediatracks/abc/0.flac"),
            "https://sp-pr-fa.audio.tidal.com/mediatracks/abc/0.flac"
        );
    }

    #[test]
    fn resolve_tidal_url_relative_path() {
        assert_eq!(
            resolve_tidal_url("mediatracks/abc/0.flac"),
            "https://sp-pr-cf.audio.tidal.com/mediatracks/abc/0.flac"
        );
    }

    #[test]
    fn resolve_tidal_url_absolute_path() {
        assert_eq!(
            resolve_tidal_url("/mediatracks/abc/0.flac"),
            "https://sp-pr-cf.audio.tidal.com/mediatracks/abc/0.flac"
        );
    }
}
