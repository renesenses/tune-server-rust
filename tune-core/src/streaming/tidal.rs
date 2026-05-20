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
const CLIENT_ID: &str = "zU4XHVVkc2tDPo4t";
const CLIENT_SECRET: &str = "VJKhDFqJPqvsPVNBV6ukXTJmwlvbttP7wlMlrc72se4=";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
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
    subscription: Option<String>,
    url_cache: Arc<Mutex<UrlCache>>,
    pending_device_auth: Option<DeviceAuthResponse>,
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
            quality: "LOSSLESS".into(),
            username: None,
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
                sample_rate: 44100,
                bit_depth: 16,
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
                .post(&format!("{AUTH_BASE}/device_authorization"))
                .form(&[("client_id", CLIENT_ID), ("scope", "r_usr+w_usr+w_sub")])
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

            return Ok(AuthStatus {
                authenticated: false,
                username: None,
                subscription: None,
                expires_at: Some(device_auth.verification_uri_complete),
            });
        }

        if let Some(ref pending) = self.pending_device_auth.clone() {
            let resp = self.client
                .post(&format!("{AUTH_BASE}/token"))
                .form(&[
                    ("client_id", CLIENT_ID),
                    ("client_secret", CLIENT_SECRET),
                    ("device_code", &pending.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("scope", "r_usr+w_usr+w_sub"),
                ])
                .send()
                .await
                .map_err(|e| format!("token: {e}"))?;

            if resp.status() == 400 {
                return Ok(AuthStatus {
                    authenticated: false,
                    username: None,
                    subscription: None,
                    expires_at: None,
                });
            }

            let token: TokenResponse = resp.json().await.map_err(|e| format!("token parse: {e}"))?;
            self.access_token = Some(token.access_token);
            self.refresh_token = token.refresh_token;
            self.token_expires = Some(Instant::now() + Duration::from_secs(token.expires_in));
            self.pending_device_auth = None;

            let me = self.api_get("/users/me").await.ok();
            self.username = me.as_ref().and_then(|v| v["username"].as_str().map(Into::into));
            self.country_code = me
                .as_ref()
                .and_then(|v| v["countryCode"].as_str().map(Into::into))
                .unwrap_or_else(|| "US".into());

            info!(username = ?self.username, "tidal_authenticated");
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

    async fn get_track_url(&self, track_id: &str, _quality: Option<&str>) -> Result<StreamUrl, String> {
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

        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self.client
            .get(&format!("{API_BASE}/tracks/{track_id}/playbackinfopostpaywall"))
            .header("Authorization", format!("Bearer {token}"))
            .query(&[
                ("audioquality", self.quality.as_str()),
                ("playbackmode", "STREAM"),
                ("assetpresentation", "FULL"),
            ])
            .send()
            .await
            .map_err(|e| format!("stream url: {e}"))?;

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;

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

        let codec = data["audioQuality"].as_str().unwrap_or("LOSSLESS");
        let (sample_rate, bit_depth) = match codec {
            "HI_RES_LOSSLESS" => (96000, 24),
            "LOSSLESS" | "HIGH" => (44100, 16),
            _ => (44100, 16),
        };

        {
            let mut cache = self.url_cache.lock().await;
            cache.set(track_id.to_string(), url.clone());
        }

        Ok(StreamUrl {
            url,
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate,
                bit_depth,
                bitrate: None,
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
        let me = self.api_get("/users/me").await?;
        let user_id = me["userId"].as_u64().ok_or("no userId")?;
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
        let me = self.api_get("/users/me").await?;
        let user_id = me["userId"].as_u64().ok_or("no userId")?;
        let data = self.api_get(&format!("/users/{user_id}/favorites/albums?limit=100")).await?;
        let albums = data["items"].as_array()
            .map(|items| items.iter().filter_map(|item| {
                item.get("item").map(Self::map_album)
            }).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        let me = self.api_get("/users/me").await?;
        let user_id = me["userId"].as_u64().ok_or("no userId")?;
        let data = self.api_get(&format!("/users/{user_id}/favorites/artists?limit=100")).await?;
        let artists = data["items"].as_array()
            .map(|items| items.iter().filter_map(|item| {
                item.get("item").map(Self::map_artist)
            }).collect())
            .unwrap_or_default();
        Ok(artists)
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
            .post(&format!("{AUTH_BASE}/token"))
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
        let token = self.access_token.as_ref()?;
        Some(serde_json::json!({
            "access_token": token,
            "refresh_token": self.refresh_token,
            "username": self.username,
            "country_code": self.country_code,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(at) = tokens["access_token"].as_str() {
            self.access_token = Some(at.into());
            self.refresh_token = tokens["refresh_token"].as_str().map(Into::into);
            self.username = tokens["username"].as_str().map(Into::into);
            self.country_code = tokens["country_code"].as_str().unwrap_or("US").into();
            true
        } else {
            false
        }
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
