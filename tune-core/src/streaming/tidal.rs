use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::traits::*;

const API_BASE: &str = "https://api.tidal.com/v1";
const AUTH_BASE: &str = "https://auth.tidal.com/v1/oauth2";
const CLIENT_ID: &str = "fX2JxdmntZWK0ixT";
const CLIENT_SECRET: &str = "1Nn9AfDAjxrgJFJbKNWLeAyKGVGmINuXPPLHVXAvxAg=";

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

struct UrlCache {
    entries: HashMap<String, (String, Instant)>,
    ttl: Duration,
}

impl UrlCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).and_then(|(url, created)| {
            if created.elapsed() < self.ttl {
                Some(url.as_str())
            } else {
                None
            }
        })
    }

    fn set(&mut self, key: String, url: String) {
        if self.entries.len() > 1000 {
            self.entries
                .retain(|_, (_, created)| created.elapsed() < self.ttl);
        }
        self.entries.insert(key, (url, Instant::now()));
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
                .unwrap(),
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
            pending_device_auth: None,
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
            .map(|t| t.iter().any(|v| v.as_str() == Some("HIRES_LOSSLESS")))
            .unwrap_or(false);
        let audio_quality = item["audioQuality"].as_str().unwrap_or(if is_hires {
            "HI_RES_LOSSLESS"
        } else {
            "LOSSLESS"
        });
        let (sample_rate, bit_depth) = match audio_quality {
            "HI_RES_LOSSLESS" => (96000, 24),
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
        serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))
    }

    fn parse_quality_metadata(data: &serde_json::Value, audio_quality: &str) -> (u32, u16) {
        if let Some(bit_depth) = data["bitDepth"].as_u64() {
            let sample_rate = data["sampleRate"].as_u64().unwrap_or(44100) as u32;
            return (sample_rate, bit_depth as u16);
        }
        match audio_quality {
            "HI_RES_LOSSLESS" => (96000, 24),
            "LOSSLESS" | "HI_RES" => (44100, 16),
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
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for TidalService {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
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
        if credentials.get("device_flow").and_then(|v| v.as_bool()) == Some(true) {
            let resp = self
                .client
                .post(format!("{AUTH_BASE}/device_authorization"))
                .form(&[("client_id", CLIENT_ID), ("scope", "r_usr w_usr w_sub")])
                .send()
                .await
                .map_err(|e| format!("device auth: {e}"))?;

            let device_auth: DeviceAuthResponse =
                resp.json().await.map_err(|e| format!("parse: {e}"))?;
            info!(
                user_code = %device_auth.user_code,
                uri = %device_auth.verification_uri_complete,
                "tidal_device_auth_started"
            );
            self.pending_device_auth = Some(device_auth.clone());

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

        info!(has_pending = self.pending_device_auth.is_some(), device_code = ?self.pending_device_auth.as_ref().map(|p| &p.device_code), "tidal_auth_poll");

        if let Some(ref pending) = self.pending_device_auth.clone() {
            let resp = self
                .client
                .post(format!("{AUTH_BASE}/token"))
                .form(&[
                    ("client_id", CLIENT_ID),
                    ("client_secret", CLIENT_SECRET),
                    ("device_code", &pending.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("scope", "r_usr w_usr w_sub"),
                ])
                .send()
                .await
                .map_err(|e| format!("token: {e}"))?;

            let status_code = resp.status().as_u16();
            if status_code != 200 {
                let body = resp.text().await.unwrap_or_default();
                info!(status = status_code, body = %body, "tidal_token_exchange");
                return Ok(AuthStatus {
                    authenticated: false,
                    ..Default::default()
                });
            }

            let token: TokenResponse =
                resp.json().await.map_err(|e| format!("token parse: {e}"))?;
            {
                let mut ts = self.tokens.lock().await;
                ts.access_token = Some(token.access_token);
                ts.refresh_token = token.refresh_token;
                ts.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
            }
            self.user_id = token.user_id;
            self.pending_device_auth = None;
            self.country_code = "FR".into();

            info!(user_id = ?self.user_id, "tidal_authenticated");
        }

        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        let ts = self.tokens.lock().await;
        AuthStatus {
            authenticated: ts.access_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            expires_at: ts.token_expires.map(|t| {
                let remaining = t.duration_since(Instant::now()).as_secs();
                format!("{remaining}s")
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
            if let Some(url) = cache.get(track_id) {
                return Ok(StreamUrl {
                    url: url.to_string(),
                    mime_type: "audio/flac".into(),
                    quality: StreamQuality {
                        codec: "FLAC".into(),
                        sample_rate: 44100,
                        bit_depth: 16,
                        bitrate: None,
                    },
                    expires_at: None,
                });
            }
        }

        let requested_quality = quality.unwrap_or(self.quality.as_str());
        let data = self
            .fetch_playback_info(track_id, requested_quality)
            .await?;

        let manifest_mime = data["manifestMimeType"].as_str().unwrap_or("");
        let has_manifest = data["manifest"].as_str().is_some();

        let data = if (!has_manifest || manifest_mime == "application/dash+xml")
            && requested_quality != "LOSSLESS"
        {
            info!(
                track_id,
                manifest_mime, has_manifest, "tidal_fallback_to_lossless"
            );
            let fallback = self.fetch_playback_info(track_id, "LOSSLESS").await?;
            if fallback["manifest"].as_str().is_none() && requested_quality != "HIGH" {
                info!(track_id, "tidal_fallback_to_high");
                self.fetch_playback_info(track_id, "HIGH").await?
            } else {
                fallback
            }
        } else {
            data
        };

        let manifest = data["manifest"].as_str().ok_or("no manifest")?;
        let decoded =
            String::from_utf8(base64_decode(manifest).map_err(|e| format!("base64: {e}"))?)
                .map_err(|e| format!("utf8: {e}"))?;

        let url = if let Ok(manifest_json) = serde_json::from_str::<serde_json::Value>(&decoded) {
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
        let mime_type = data["manifestMimeType"]
            .as_str()
            .and_then(|m| if m.contains("dash") { None } else { Some(m) })
            .unwrap_or("audio/flac");

        {
            let mut cache = self.url_cache.lock().await;
            cache.set(track_id.to_string(), url.clone());
        }

        info!(
            track_id,
            audio_quality, sample_rate, bit_depth, "tidal_stream_url"
        );

        Ok(StreamUrl {
            url,
            mime_type: mime_type.to_string(),
            quality: StreamQuality {
                codec: if audio_quality.contains("LOSSLESS") {
                    "FLAC"
                } else {
                    "AAC"
                }
                .into(),
                sample_rate,
                bit_depth,
                bitrate: data["bitDepth"]
                    .as_u64()
                    .map(|_| sample_rate * (bit_depth as u32) * 2),
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
            _ => return Err(format!("unknown favorite type: {fav_type}")),
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
            _ => return Err(format!("unknown favorite type: {fav_type}")),
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
            restored = true;
        }
        restored
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
}
