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

const API_BASE: &str = "https://api.tidal.com/v1";
const AUTH_BASE: &str = "https://auth.tidal.com/v1/oauth2";
/// Audirvana desktop client ID — uses OAuth PKCE (no client_secret needed).
/// The old Android Automotive client (zU4XHVVkc2tDPo4t) caused 15s token expiry.
const CLIENT_ID: &str = "C2B7SpVY5qTN6jbJ";
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

/// PKCE state stored while waiting for the user to complete the OAuth flow.
#[derive(Debug, Clone)]
struct PkceState {
    code_verifier: String,
    authorize_url: String,
    started: Instant,
}

/// Mutable token state behind a Mutex so `&self` methods can refresh on 401.
struct TokenState {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_expires: Option<Instant>,
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
    pending_device_auth: Option<DeviceAuthResponse>,
    device_auth_started: Option<Instant>,
    /// PKCE OAuth flow state — stored while user completes browser login.
    pending_pkce: Option<PkceState>,
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
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            tokens: Mutex::new(TokenState {
                access_token: None,
                refresh_token: None,
                token_expires: None,
            }),
            country_code: "US".into(),
            quality: "HI_RES".into(),
            username: None,
            user_id: None,
            subscription: None,
            url_cache: Arc::new(Mutex::new(UrlCache::new(240))),
            pending_device_auth: None,
            device_auth_started: None,
            pending_pkce: None,
            featured_cache: None,
            enabled_override: None,
        }
    }

    /// Get current access token from the token state.
    async fn get_access_token(&self) -> Result<String, String> {
        let ts = self.tokens.lock().await;
        ts.access_token
            .clone()
            .ok_or_else(|| "not authenticated".into())
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
            quality: None,
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
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("stream url: {e}"))?;

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

    /// Generate a PKCE code_verifier (43-128 URL-safe random chars).
    fn generate_code_verifier() -> String {
        // Use multiple UUID v4 values concatenated (each is 32 hex chars)
        // to get 64 URL-safe random characters.
        let u1 = uuid::Uuid::new_v4().simple().to_string();
        let u2 = uuid::Uuid::new_v4().simple().to_string();
        format!("{u1}{u2}") // 64 chars, all hex (URL-safe)
    }

    /// Compute the PKCE code_challenge = BASE64URL(SHA256(code_verifier)).
    fn compute_code_challenge(verifier: &str) -> String {
        let hash = Sha256::digest(verifier.as_bytes());
        base64_encode_url(&hash)
    }

    /// Build the PKCE authorize URL that the user opens in their browser.
    fn build_authorize_url(code_challenge: &str) -> String {
        format!(
            "https://login.tidal.com/authorize?\
             client_id={CLIENT_ID}\
             &response_type=code\
             &redirect_uri={}\
             &code_challenge={code_challenge}\
             &code_challenge_method=S256\
             &scope=r_usr+w_usr",
            urlencoding::encode(REDIRECT_URI),
        )
    }

    /// Exchange an authorization code for tokens using the PKCE code_verifier.
    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, String> {
        let resp = self
            .client
            .post(format!("{AUTH_BASE}/token"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", code_verifier),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await
            .map_err(|e| format!("token exchange: {e}"))?;

        let status_code = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
        if status_code != 200 {
            warn!(status = status_code, body = %body, "tidal_pkce_token_exchange_failed");
            return Err(format!("token exchange failed: {status_code} {body}"));
        }

        info!(body = %body, "tidal_pkce_token_exchange_success");
        serde_json::from_str(&body).map_err(|e| {
            warn!(error = %e, body = %body, "tidal_pkce_token_parse_failed");
            format!("token parse: {e}")
        })
    }

    /// Public method for the route handler to call when the OAuth callback arrives.
    pub async fn handle_oauth_callback(&mut self, code: &str) -> Result<AuthStatus, String> {
        let code_verifier = self
            .pending_pkce
            .as_ref()
            .map(|p| p.code_verifier.clone())
            .ok_or("no pending PKCE flow — initiate auth first")?;

        let token = self.exchange_code(code, &code_verifier).await?;
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
        self.pending_pkce = None;
        self.pending_device_auth = None;
        self.device_auth_started = None;
        self.country_code = "FR".into();

        info!(user_id = ?self.user_id, "tidal_pkce_authenticated");

        // Fetch username and subscription info
        self.refresh_user_info().await;

        Ok(self.auth_status().await)
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
    ) -> Result<AuthStatus, String> {
        // --- Handle OAuth callback code (from redirect) ---
        if let Some(code) = credentials.get("code").and_then(|v| v.as_str()) {
            return self.handle_oauth_callback(code).await;
        }

        // --- Initiate PKCE flow (when device_flow: true or first auth call) ---
        if credentials.get("device_flow").and_then(|v| v.as_bool()) == Some(true) {
            let code_verifier = Self::generate_code_verifier();
            let code_challenge = Self::compute_code_challenge(&code_verifier);
            let authorize_url = Self::build_authorize_url(&code_challenge);

            info!(
                url = %authorize_url,
                "tidal_pkce_auth_started"
            );

            self.pending_pkce = Some(PkceState {
                code_verifier,
                authorize_url: authorize_url.clone(),
                started: Instant::now(),
            });
            // Clear legacy device auth state
            self.pending_device_auth = None;
            self.device_auth_started = None;

            return Ok(AuthStatus {
                authenticated: false,
                verification_url: Some(authorize_url),
                user_code: None, // PKCE doesn't use user codes
                ..Default::default()
            });
        }

        // --- Poll: check if PKCE flow is pending (return URL again for UI) ---
        if let Some(ref pkce) = self.pending_pkce {
            // Check if the PKCE flow has been pending too long (10 minutes)
            if pkce.started.elapsed() > Duration::from_secs(600) {
                warn!("tidal_pkce_flow_expired — clearing pending auth");
                self.pending_pkce = None;
                return Ok(AuthStatus {
                    authenticated: false,
                    ..Default::default()
                });
            }
            // Return the URL again so the UI can show it
            return Ok(AuthStatus {
                authenticated: false,
                verification_url: Some(pkce.authorize_url.clone()),
                ..Default::default()
            });
        }

        Ok(self.auth_status().await)
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

    async fn logout(&mut self) -> Result<(), String> {
        {
            let mut ts = self.tokens.lock().await;
            ts.access_token = None;
            ts.refresh_token = None;
        }
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String> {
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

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, String> {
        let data = self.api_get(&format!("/tracks/{track_id}")).await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, String> {
        {
            let cache = self.url_cache.lock().await;
            if let Some(cached) = cache.get(track_id) {
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

                    debug!(
                        track_id,
                        requested = q,
                        returned = returned_quality,
                        returned_codec,
                        manifest_mime,
                        ?bit_depth,
                        ?sample_rate,
                        "tidal_playback_info_response"
                    );

                    if returned_quality == q {
                        // Got exactly what we requested — use it
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

        let url = if manifest_mime == "application/dash+xml" {
            // DASH manifest (MPD XML) — extract the direct stream URL from <BaseURL>
            extract_dash_base_url(&decoded).ok_or_else(|| {
                format!("tidal: could not extract BaseURL from DASH manifest for track {track_id}")
            })?
        } else if let Ok(manifest_json) = serde_json::from_str::<serde_json::Value>(&decoded) {
            // BTS manifest (JSON with urls array)
            manifest_json["urls"]
                .as_array()
                .and_then(|urls| urls.first())
                .and_then(|u| u.as_str())
                .ok_or("no url in manifest")?
                .to_string()
        } else {
            decoded
        };

        let audio_quality = data["audioQuality"].as_str().unwrap_or("LOSSLESS");
        let (sample_rate, bit_depth) = Self::parse_quality_metadata(&data, audio_quality);

        // Determine codec from the audio quality level:
        // HI_RES_LOSSLESS = FLAC Hi-Res, HI_RES = MQA/FLAC, LOSSLESS = FLAC CD
        // Only HIGH and below are AAC
        let codec = match audio_quality {
            "HI_RES_LOSSLESS" | "HI_RES" | "LOSSLESS" => "FLAC",
            _ => "AAC",
        };

        let mime_type = if codec == "FLAC" {
            "audio/flac"
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

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, String> {
        let data = self.api_get(&format!("/albums/{album_id}")).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self
            .api_get(&format!("/albums/{album_id}/tracks?limit=100"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, String> {
        let data = self.api_get(&format!("/artists/{artist_id}")).await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let data = self
            .api_get(&format!("/artists/{artist_id}/albums?limit=50"))
            .await?;
        let albums = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self
            .api_get(&format!("/artists/{artist_id}/toptracks?limit=20"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, String> {
        let data = self.api_get(&format!("/playlists/{playlist_id}")).await?;
        Ok(Self::map_playlist(&data))
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self
            .api_get(&format!("/playlists/{playlist_id}/tracks?limit=100"))
            .await?;
        let tracks = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_genres(&self) -> Result<Vec<StreamGenre>, String> {
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
    ) -> Result<Vec<StreamAlbum>, String> {
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

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, String> {
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

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, String> {
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

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, String> {
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

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
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

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let user_id = self.user_id.ok_or("no user_id")?;
        let path_type = match fav_type {
            "tracks" => "tracks",
            "albums" => "albums",
            "artists" => "artists",
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_delete(&format!("/users/{user_id}/favorites/{path_type}/{item_id}"))
            .await
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
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
    ) -> Result<String, String> {
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
    ) -> Result<usize, String> {
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

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
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

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
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

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, String> {
        let data = self.api_get("/featured/playlists?limit=50").await?;
        let playlists = data["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_playlist).collect())
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, String> {
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

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
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
        self.do_refresh_token().await
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
        // PKCE code_verifier is ephemeral — not persisted across restarts.
        // If user was mid-flow when server restarted, they just re-initiate.
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
        // Legacy device_code state is ignored — PKCE flow is ephemeral and
        // users simply re-initiate if the server restarts mid-flow.
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

fn base64_decode_url(input: &str) -> Result<Vec<u8>, String> {
    let padded = match input.len() % 4 {
        2 => format!("{input}=="),
        3 => format!("{input}="),
        _ => input.to_string(),
    };
    let standard = padded.replace('-', "+").replace('_', "/");
    base64_decode(&standard)
}

/// Encode bytes to URL-safe Base64 without padding (RFC 7636 for PKCE).
fn base64_encode_url(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut i = 0;
    while i + 2 < input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        output.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        output.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        output.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = input.len() - i;
    if remaining == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        output.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        output.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
    } else if remaining == 1 {
        let n = (input[i] as u32) << 16;
        output.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
    }
    // Convert to URL-safe (no padding, - instead of +, _ instead of /)
    output.replace('+', "-").replace('/', "_")
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

fn extract_dash_base_url(mpd: &str) -> Option<String> {
    // Look for <BaseURL>...</BaseURL> — simple XML extraction without
    // pulling in a full XML parser dependency.
    let start_tag = "<BaseURL>";
    let end_tag = "</BaseURL>";
    let start = mpd.find(start_tag)? + start_tag.len();
    let end = mpd[start..].find(end_tag)? + start;
    let url = mpd[start..end].trim();
    if url.starts_with("http") {
        Some(url.to_string())
    } else {
        None
    }
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
    fn extract_dash_base_url_non_http() {
        let mpd = "<MPD><BaseURL>relative/path.flac</BaseURL></MPD>";
        assert!(extract_dash_base_url(mpd).is_none());
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

    #[test]
    fn pkce_code_verifier_length() {
        let verifier = TidalService::generate_code_verifier();
        // Must be 43-128 chars per RFC 7636
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
        // Must be URL-safe (hex chars only in our implementation)
        assert!(verifier.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn pkce_code_challenge_deterministic() {
        let verifier = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ";
        let challenge = TidalService::compute_code_challenge(verifier);
        // Must be URL-safe base64 (no +, /, or = padding)
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
        // SHA-256 produces 32 bytes → base64 = 43 chars (without padding)
        assert_eq!(challenge.len(), 43);
        // Deterministic — same input gives same output
        assert_eq!(challenge, TidalService::compute_code_challenge(verifier));
    }

    #[test]
    fn pkce_authorize_url_format() {
        let challenge = "test_challenge_value";
        let url = TidalService::build_authorize_url(challenge);
        assert!(url.starts_with("https://login.tidal.com/authorize?"));
        assert!(url.contains("client_id=C2B7SpVY5qTN6jbJ"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=test_challenge_value"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=r_usr+w_usr"));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn base64_encode_url_known_value() {
        // Known SHA-256("hello") encoded in URL-safe base64 (no padding)
        let hash = Sha256::digest(b"hello");
        let encoded = base64_encode_url(&hash);
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
        assert_eq!(encoded.len(), 43); // 32 bytes → 43 base64url chars
    }
}
