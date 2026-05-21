use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

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
        self.entries.insert(key, (url, Instant::now()));
    }
}

pub struct TidalService {
    client: Client,
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_expires: Option<Instant>,
    country_code: String,
    quality: String,
    username: Option<String>,
    user_id: Option<u64>,
    subscription: Option<String>,
    url_cache: Arc<Mutex<UrlCache>>,
    pending_device_auth: Option<DeviceAuthResponse>,
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
            access_token: None,
            refresh_token: None,
            token_expires: None,
            country_code: "US".into(),
            quality: "HI_RES_LOSSLESS".into(),
            username: None,
            user_id: None,
            subscription: None,
            url_cache: Arc::new(Mutex::new(UrlCache::new(240))),
            pending_device_auth: None,
        }
    }

    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let url = format!("{API_BASE}{path}");
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&[("countryCode", &self.country_code)])
            .send()
            .await
            .map_err(|e| format!("tidal api: {e}"))?;

        if resp.status() == 401 {
            return Err("token expired".into());
        }

        resp.json().await.map_err(|e| format!("tidal json: {e}"))
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let tags = item["mediaMetadata"]["tags"].as_array();
        let is_hires = tags
            .map(|t| t.iter().any(|v| v.as_str() == Some("HIRES_LOSSLESS")))
            .unwrap_or(false);
        let audio_quality = item["audioQuality"].as_str().unwrap_or(
            if is_hires { "HI_RES_LOSSLESS" } else { "LOSSLESS" }
        );
        let (sample_rate, bit_depth) = match audio_quality {
            "HI_RES_LOSSLESS" => (96000, 24),
            _ => (44100, 16),
        };

        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str()
                .or_else(|| item["artists"].as_array().and_then(|a| a.first()).and_then(|a| a["name"].as_str()))
                .unwrap_or("").into(),
            album: item["album"]["title"].as_str().map(Into::into),
            album_id: item["album"]["id"].as_u64().map(|id| id.to_string()),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_url: item["album"]["cover"].as_str().map(|c| {
                format!("https://resources.tidal.com/images/{}/640x640.jpg", c.replace('-', "/"))
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
            artist: item["artist"]["name"].as_str()
                .or_else(|| item["artists"].as_array().and_then(|a| a.first()).and_then(|a| a["name"].as_str()))
                .unwrap_or("").into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_url: item["cover"].as_str().map(|c| {
                format!("https://resources.tidal.com/images/{}/640x640.jpg", c.replace('-', "/"))
            }),
            year: item["releaseDate"].as_str().and_then(|d| d.get(..4)?.parse().ok()),
            track_count: item["numberOfTracks"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    async fn fetch_playback_info(&self, track_id: &str, quality: &str) -> Result<serde_json::Value, String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self.client
            .get(format!("{API_BASE}/tracks/{track_id}/playbackinfopostpaywall"))
            .header("Authorization", format!("Bearer {token}"))
            .query(&[
                ("audioquality", quality),
                ("playbackmode", "STREAM"),
                ("assetpresentation", "FULL"),
            ])
            .send()
            .await
            .map_err(|e| format!("stream url: {e}"))?;

        if resp.status() == 429 {
            return Err("tidal rate limited".into());
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

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_url: item["picture"].as_str().map(|p| {
                format!("https://resources.tidal.com/images/{}/480x480.jpg", p.replace('-', "/"))
            }),
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for TidalService {
    fn name(&self) -> &str {
        "tidal"
    }

    fn enabled(&self) -> bool {
        true
    }

    async fn authenticate(&mut self, credentials: &serde_json::Value) -> Result<AuthStatus, String> {
        if credentials.get("device_flow").and_then(|v| v.as_bool()) == Some(true) {
            let resp = self.client
                .post(format!("{AUTH_BASE}/device_authorization"))
                .form(&[("client_id", CLIENT_ID), ("scope", "r_usr w_usr w_sub")])
                .send()
                .await
                .map_err(|e| format!("device auth: {e}"))?;

            let device_auth: DeviceAuthResponse = resp.json().await.map_err(|e| format!("parse: {e}"))?;
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
            let resp = self.client
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

            let token: TokenResponse = resp.json().await.map_err(|e| format!("token parse: {e}"))?;
            self.access_token = Some(token.access_token);
            self.refresh_token = token.refresh_token;
            self.user_id = token.user_id;
            self.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
            self.pending_device_auth = None;
            self.country_code = "FR".into();

            info!(user_id = ?self.user_id, "tidal_authenticated");
        }

        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.access_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            expires_at: self.token_expires.map(|t| {
                let remaining = t.duration_since(Instant::now()).as_secs();
                format!("{remaining}s")
            }),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), String> {
        self.access_token = None;
        self.refresh_token = None;
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String> {
        let data = self.api_get(&format!(
            "/search?query={}&limit={limit}&types=TRACKS,ALBUMS,ARTISTS",
            urlencoding::encode(query)
        )).await?;

        let tracks = data["tracks"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        let artists = data["artists"]["items"].as_array()
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

    async fn get_track_url(&self, track_id: &str, quality: Option<&str>) -> Result<StreamUrl, String> {
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
        let data = self.fetch_playback_info(track_id, requested_quality).await?;

        let manifest_mime = data["manifestMimeType"].as_str().unwrap_or("");
        let has_manifest = data["manifest"].as_str().is_some();

        let data = if (!has_manifest || manifest_mime == "application/dash+xml") && requested_quality != "LOSSLESS" {
            info!(track_id, manifest_mime, has_manifest, "tidal_fallback_to_lossless");
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
        let decoded = String::from_utf8(
            base64_decode(manifest).map_err(|e| format!("base64: {e}"))?
        ).map_err(|e| format!("utf8: {e}"))?;

        let url = if let Ok(manifest_json) = serde_json::from_str::<serde_json::Value>(&decoded) {
            manifest_json["urls"].as_array()
                .and_then(|urls| urls.first())
                .and_then(|u| u.as_str())
                .ok_or("no url in manifest")?
                .to_string()
        } else {
            decoded
        };

        let audio_quality = data["audioQuality"].as_str().unwrap_or("LOSSLESS");
        let (sample_rate, bit_depth) = Self::parse_quality_metadata(&data, audio_quality);
        let mime_type = data["manifestMimeType"].as_str()
            .and_then(|m| if m.contains("dash") { None } else { Some(m) })
            .unwrap_or("audio/flac");

        {
            let mut cache = self.url_cache.lock().await;
            cache.set(track_id.to_string(), url.clone());
        }

        info!(track_id, audio_quality, sample_rate, bit_depth, "tidal_stream_url");

        Ok(StreamUrl {
            url,
            mime_type: mime_type.to_string(),
            quality: StreamQuality {
                codec: if audio_quality.contains("LOSSLESS") { "FLAC" } else { "AAC" }.into(),
                sample_rate,
                bit_depth,
                bitrate: data["bitDepth"].as_u64().map(|_| {
                    sample_rate * (bit_depth as u32) * 2
                }),
            },
            expires_at: None,
        })
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, String> {
        let data = self.api_get(&format!("/albums/{album_id}")).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get(&format!("/albums/{album_id}/tracks?limit=100")).await?;
        let tracks = data["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, String> {
        let data = self.api_get(&format!("/artists/{artist_id}")).await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, String> {
        let data = self.api_get(&format!("/playlists/{playlist_id}")).await?;
        Ok(StreamPlaylist {
            id: data["uuid"].as_str().unwrap_or(playlist_id).into(),
            name: data["title"].as_str().unwrap_or("").into(),
            description: data["description"].as_str().map(Into::into),
            cover_url: data["squareImage"].as_str().map(|c| {
                format!("https://resources.tidal.com/images/{}/640x640.jpg", c.replace('-', "/"))
            }),
            track_count: data["numberOfTracks"].as_u64().unwrap_or(0) as u32,
            owner: data["creator"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get(&format!("/playlists/{playlist_id}/tracks?limit=100")).await?;
        let tracks = data["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        // Use stored user_id instead of /users/me
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self.api_get(&format!("/users/{user_id}/playlists?limit=50")).await?;
        let playlists = data["items"].as_array()
            .map(|items| items.iter().map(|item| StreamPlaylist {
                id: item["uuid"].as_str().unwrap_or("").into(),
                name: item["title"].as_str().unwrap_or("").into(),
                description: item["description"].as_str().map(Into::into),
                cover_url: None,
                track_count: item["numberOfTracks"].as_u64().unwrap_or(0) as u32,
                owner: None,
            }).collect())
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
        // Use stored user_id instead of /users/me
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self.api_get(&format!("/users/{user_id}/favorites/albums?limit=100")).await?;
        let albums = data["items"].as_array()
            .map(|items| items.iter().filter_map(|item| {
                item.get("item").map(Self::map_album)
            }).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        // Use stored user_id instead of /users/me
        let user_id = self.user_id.ok_or("no user_id — re-authenticate")?;
        let data = self.api_get(&format!("/users/{user_id}/favorites/artists?limit=100")).await?;
        let artists = data["items"].as_array()
            .map(|items| items.iter().filter_map(|item| {
                item.get("item").map(Self::map_artist)
            }).collect())
            .unwrap_or_default();
        Ok(artists)
    }

    async fn post_restore(&mut self) {
        self.refresh_user_info().await;
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        let needs_refresh = self
            .token_expires
            .map(|exp| {
                exp.checked_duration_since(Instant::now())
                    .map(|d| d.as_secs() < 300)
                    .unwrap_or(true)
            })
            .unwrap_or(false);

        if !needs_refresh {
            return Ok(false);
        }

        let refresh_token = match self.refresh_token.as_ref() {
            Some(rt) => rt.clone(),
            None => return Ok(false),
        };

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
            return Err("refresh token rejected".into());
        }

        let token: TokenResponse = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        self.access_token = Some(token.access_token);
        if let Some(rt) = token.refresh_token {
            self.refresh_token = Some(rt);
        }
        self.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
        info!("tidal_token_refreshed");
        Ok(true)
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        let mut obj = serde_json::json!({});
        if let Some(ref token) = self.access_token {
            obj["access_token"] = serde_json::json!(token);
            obj["refresh_token"] = serde_json::json!(self.refresh_token);
            obj["username"] = serde_json::json!(self.username);
            obj["country_code"] = serde_json::json!(self.country_code);
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
            self.access_token = Some(at.into());
            self.refresh_token = tokens["refresh_token"].as_str().map(Into::into);
            self.username = tokens["username"].as_str().map(Into::into);
            self.country_code = tokens["country_code"].as_str().unwrap_or("FR").into();
            self.user_id = tokens["user_id"].as_u64();
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
        if byte == b'=' { break; }
        let val = table.iter().position(|&c| c == byte).ok_or("invalid base64")? as u32;
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
