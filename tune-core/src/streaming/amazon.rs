use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::traits::*;
use crate::TuneError;

const AMAZON_AUTH_URL: &str = "https://api.amazon.com/auth/o2/";
const AMAZON_TUNE_API: &str = "https://music.amazon.com/api/";
const AMAZON_CLIENT_ID: &str = "amzn1.application-oa2-client.music";
const STREAM_URL_TTL: u64 = 600;

struct CachedUrl {
    url: String,
    expires: Instant,
}

pub struct AmazonMusicService {
    client: Client,
    access_token: Option<String>,
    refresh_token: Option<String>,
    device_id: String,
    region: String,
    quality: String,
    enabled_override: Option<bool>,
    url_cache: Mutex<HashMap<String, CachedUrl>>,
    device_code: Mutex<Option<String>>,
    verification_url: Mutex<Option<String>>,
    user_code: Mutex<Option<String>>,
}

impl Default for AmazonMusicService {
    fn default() -> Self {
        Self::new()
    }
}

impl AmazonMusicService {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            access_token: None,
            refresh_token: None,
            device_id: uuid::Uuid::new_v4().to_string(),
            region: "fr".into(),
            quality: "HD".into(),
            enabled_override: None,
            url_cache: Mutex::new(HashMap::new()),
            device_code: Mutex::new(None),
            verification_url: Mutex::new(None),
            user_code: Mutex::new(None),
        }
    }

    fn region_tld(&self) -> &str {
        match self.region.as_str() {
            "uk" => "co.uk",
            "de" => "de",
            "fr" => "fr",
            "it" => "it",
            "es" => "es",
            "jp" => "co.jp",
            "ca" => "ca",
            "au" => "com.au",
            "br" => "com.br",
            "mx" => "com.mx",
            "in" => "in",
            _ => "com",
        }
    }

    fn api_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(ref token) = self.access_token {
            if let Ok(val) = format!("Bearer {token}").parse() {
                headers.insert("Authorization", val);
            }
        }
        if let Ok(val) = self.device_id.parse() {
            headers.insert("X-Amzn-Device-Id", val);
        }
        if let Ok(val) = format!("music.amazon.{}", self.region_tld()).parse() {
            headers.insert("X-Amzn-Music-Domain", val);
        }
        headers
    }

    async fn api_get(&self, endpoint: &str) -> Result<Value, String> {
        let url = format!("{AMAZON_TUNE_API}{endpoint}");
        let resp = self
            .client
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await
            .map_err(|e| format!("amazon api: {e}"))?;
        if resp.status().as_u16() == 401 {
            return Err("unauthorized".into());
        }
        resp.json().await.map_err(|e| format!("amazon parse: {e}"))
    }

    async fn api_post(&self, endpoint: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{AMAZON_TUNE_API}{endpoint}");
        let resp = self
            .client
            .post(&url)
            .headers(self.api_headers())
            .json(body)
            .send()
            .await
            .map_err(|e| format!("amazon api: {e}"))?;
        if resp.status().as_u16() == 401 {
            return Err("unauthorized".into());
        }
        resp.json().await.map_err(|e| format!("amazon parse: {e}"))
    }

    async fn refresh_access_token(&mut self) -> bool {
        let refresh = match &self.refresh_token {
            Some(r) => r.clone(),
            None => return false,
        };
        let resp = self
            .client
            .post(format!("{AMAZON_AUTH_URL}token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", AMAZON_CLIENT_ID),
            ])
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let data: Value = r.json().await.unwrap_or_default();
                self.access_token = data["access_token"].as_str().map(Into::into);
                if let Some(new_refresh) = data["refresh_token"].as_str() {
                    self.refresh_token = Some(new_refresh.into());
                }
                info!("amazon_token_refreshed");
                self.access_token.is_some()
            }
            _ => {
                warn!("amazon_token_refresh_failed");
                false
            }
        }
    }

    fn map_track(&self, data: &Value) -> StreamTrack {
        let artist = data["artist"]
            .as_object()
            .and_then(|o| o["name"].as_str())
            .or_else(|| data["artistName"].as_str())
            .unwrap_or("Unknown")
            .to_string();
        let album = data["album"]
            .as_object()
            .and_then(|o| o["title"].as_str())
            .or_else(|| data["albumTitle"].as_str())
            .map(|s| s.to_string());
        let duration_s = data["duration"].as_f64().unwrap_or(0.0);
        let (sr, bd) = match self.quality.as_str() {
            "ULTRA_HD" => (96000, 24),
            _ => (44100, 16),
        };
        StreamTrack {
            id: data["id"]
                .as_str()
                .or_else(|| data["trackId"].as_str())
                .unwrap_or("")
                .to_string(),
            title: data["title"].as_str().unwrap_or("").to_string(),
            artist,
            album,
            album_id: data["albumId"].as_str().map(Into::into),
            duration_ms: (duration_s * 1000.0) as u64,
            cover_path: data["artworkUrl"].as_str().map(Into::into),
            track_number: data["trackNumber"].as_u64().map(|n| n as u32),
            disc_number: data["discNumber"].as_u64().map(|n| n as u32),
            explicit: data["explicit"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality {
                codec: if self.quality == "SD" {
                    "AAC".into()
                } else {
                    "FLAC".into()
                },
                sample_rate: sr,
                bit_depth: bd,
                bitrate: None,
                channels: 2,
            }),
        }
    }

    fn map_album(&self, data: &Value) -> StreamAlbum {
        let artist = data["artist"]
            .as_object()
            .and_then(|o| o["name"].as_str())
            .or_else(|| data["artistName"].as_str())
            .unwrap_or("Unknown")
            .to_string();
        StreamAlbum {
            id: data["id"]
                .as_str()
                .or_else(|| data["albumId"].as_str())
                .unwrap_or("")
                .to_string(),
            title: data["title"].as_str().unwrap_or("").to_string(),
            artist,
            artist_id: data["artistId"].as_str().map(Into::into),
            cover_path: data["artworkUrl"].as_str().map(Into::into),
            year: data["year"].as_u64().map(|y| y as u32),
            track_count: data["trackCount"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    fn map_artist(&self, data: &Value) -> StreamArtist {
        StreamArtist {
            id: data["id"]
                .as_str()
                .or_else(|| data["artistId"].as_str())
                .unwrap_or("")
                .to_string(),
            name: data["name"]
                .as_str()
                .or_else(|| data["artistName"].as_str())
                .unwrap_or("Unknown")
                .to_string(),
            image_path: data["artworkUrl"].as_str().map(Into::into),
        }
    }

    async fn get_cached_url(&self, track_id: &str) -> Option<String> {
        let cache = self.url_cache.lock().await;
        cache.get(track_id).and_then(|c| {
            if c.expires > Instant::now() {
                Some(c.url.clone())
            } else {
                None
            }
        })
    }

    async fn cache_url(&self, track_id: &str, url: &str) {
        let mut cache = self.url_cache.lock().await;
        cache.insert(
            track_id.to_string(),
            CachedUrl {
                url: url.to_string(),
                expires: Instant::now() + Duration::from_secs(STREAM_URL_TTL),
            },
        );
    }
}

#[async_trait::async_trait]
impl StreamingService for AmazonMusicService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "amazon"
    }
    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(self.access_token.is_some())
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(&mut self, _credentials: &Value) -> Result<AuthStatus, TuneError> {
        // Device code flow — step 1: request code pair
        let resp = self
            .client
            .post(format!("{AMAZON_AUTH_URL}create/codepair"))
            .form(&[
                ("response_type", "device_code"),
                ("client_id", AMAZON_CLIENT_ID),
                ("scope", "music::playback music::library"),
            ])
            .send()
            .await
            .map_err(|e| format!("amazon auth: {e}"))?;

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("amazon auth parse: {e}"))?;
        let device_code = data["device_code"]
            .as_str()
            .ok_or("no device_code")?
            .to_string();
        let user_code = data["user_code"]
            .as_str()
            .ok_or("no user_code")?
            .to_string();
        let verification_url = data["verification_uri"]
            .as_str()
            .unwrap_or("https://www.amazon.com/code")
            .to_string();

        *self.device_code.lock().await = Some(device_code.clone());
        *self.verification_url.lock().await = Some(verification_url.clone());
        *self.user_code.lock().await = Some(user_code.clone());

        info!(user_code = %user_code, "amazon_auth_started");

        Ok(AuthStatus {
            authenticated: false,
            username: None,
            subscription: None,
            expires_at: None,
            verification_url: Some(verification_url),
            user_code: Some(user_code),
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        // Poll for token if device_code is set
        let _device_code = self.device_code.lock().await.clone();
        AuthStatus {
            authenticated: self.access_token.is_some(),
            username: None,
            subscription: None,
            expires_at: None,
            verification_url: self.verification_url.lock().await.clone(),
            user_code: self.user_code.lock().await.clone(),
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.access_token = None;
        self.refresh_token = None;
        *self.device_code.lock().await = None;
        *self.url_cache.lock().await = HashMap::new();
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        if self.access_token.is_none() {
            return Ok(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
                playlists: vec![],
            });
        }
        let data = self
            .api_get(&format!(
                "search?query={}&limit={limit}",
                urlencoding::encode(query)
            ))
            .await?;
        let results = data["results"].as_array().cloned().unwrap_or_default();

        let mut tracks = Vec::new();
        let mut albums = Vec::new();
        let mut artists = Vec::new();
        for r in &results {
            match r["type"].as_str() {
                Some("track") => tracks.push(self.map_track(r)),
                Some("album") => albums.push(self.map_album(r)),
                Some("artist") => artists.push(self.map_artist(r)),
                _ => {}
            }
        }
        Ok(SearchResults {
            tracks,
            albums,
            artists,
            playlists: vec![],
        })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        let data = self.api_get(&format!("tracks/{track_id}")).await?;
        Ok(self.map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        if let Some(cached) = self.get_cached_url(track_id).await {
            let (sr, bd) = if self.quality == "ULTRA_HD" {
                (96000, 24)
            } else {
                (44100, 16)
            };
            return Ok(StreamUrl {
                url: cached,
                mime_type: if self.quality == "SD" {
                    "audio/aac".into()
                } else {
                    "audio/flac".into()
                },
                quality: StreamQuality {
                    codec: if self.quality == "SD" { "AAC" } else { "FLAC" }.into(),
                    sample_rate: sr,
                    bit_depth: bd,
                    bitrate: None,
                    channels: 2,
                },
                expires_at: None,
            });
        }

        let body = json!({
            "trackId": track_id,
            "quality": &self.quality,
            "deviceId": &self.device_id,
        });
        let data = self.api_post("stream", &body).await?;
        let url = data["url"]
            .as_str()
            .or_else(|| data["streamUrl"].as_str())
            .ok_or("no stream url")?
            .to_string();

        self.cache_url(track_id, &url).await;

        let (sr, bd) = if self.quality == "ULTRA_HD" {
            (96000, 24)
        } else {
            (44100, 16)
        };
        Ok(StreamUrl {
            url,
            mime_type: if self.quality == "SD" {
                "audio/aac".into()
            } else {
                "audio/flac".into()
            },
            quality: StreamQuality {
                codec: if self.quality == "SD" { "AAC" } else { "FLAC" }.into(),
                sample_rate: sr,
                bit_depth: bd,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
        })
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        let data = self.api_get(&format!("albums/{album_id}")).await?;
        Ok(self.map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self.api_get(&format!("albums/{album_id}/tracks")).await?;
        let items = data.as_array().cloned().unwrap_or_default();
        Ok(items.iter().map(|t| self.map_track(t)).collect())
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        let data = self.api_get(&format!("artists/{artist_id}")).await?;
        Ok(self.map_artist(&data))
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self.api_get(&format!("artists/{artist_id}/albums")).await?;
        let items = data.as_array().cloned().unwrap_or_default();
        Ok(items.iter().map(|a| self.map_album(a)).collect())
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self.api_get(&format!("artists/{artist_id}/tracks")).await?;
        let items = data.as_array().cloned().unwrap_or_default();
        Ok(items.iter().map(|t| self.map_track(t)).collect())
    }

    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("not implemented".into())
    }

    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("not implemented".into())
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }

    fn save_tokens(&self) -> Option<Value> {
        let refresh = self.refresh_token.as_ref()?;
        Some(json!({
            "access_token": self.access_token,
            "refresh_token": refresh,
            "device_id": self.device_id,
            "region": self.region,
            "quality": self.quality,
        }))
    }

    fn restore_tokens(&mut self, tokens: &Value) -> bool {
        self.access_token = tokens["access_token"].as_str().map(Into::into);
        self.refresh_token = tokens["refresh_token"].as_str().map(Into::into);
        if let Some(did) = tokens["device_id"].as_str() {
            self.device_id = did.to_string();
        }
        if let Some(r) = tokens["region"].as_str() {
            self.region = r.to_string();
        }
        if let Some(q) = tokens["quality"].as_str() {
            self.quality = q.to_string();
        }
        self.refresh_token.is_some()
    }

    async fn post_restore(&mut self) {
        if self.refresh_token.is_some() && !self.refresh_access_token().await {
            self.access_token = None;
            self.refresh_token = None;
            warn!("amazon_restore_token_invalid");
        }
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        if self.access_token.is_some() && self.refresh_token.is_some() {
            Ok(self.refresh_access_token().await)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_tld_mapping() {
        let svc = AmazonMusicService::new();
        assert_eq!(svc.region_tld(), "fr");
    }

    #[test]
    fn map_track_basic() {
        let svc = AmazonMusicService::new();
        let data = json!({
            "id": "B00001",
            "title": "Test Track",
            "artistName": "Test Artist",
            "albumTitle": "Test Album",
            "duration": 180.5,
        });
        let track = svc.map_track(&data);
        assert_eq!(track.id, "B00001");
        assert_eq!(track.title, "Test Track");
        assert_eq!(track.artist, "Test Artist");
        assert_eq!(track.duration_ms, 180500);
    }

    #[test]
    fn save_restore_tokens() {
        let mut svc = AmazonMusicService::new();
        svc.access_token = Some("at".into());
        svc.refresh_token = Some("rt".into());
        svc.region = "de".into();
        let tokens = svc.save_tokens().unwrap();

        let mut svc2 = AmazonMusicService::new();
        assert!(svc2.restore_tokens(&tokens));
        assert_eq!(svc2.access_token.as_deref(), Some("at"));
        assert_eq!(svc2.region, "de");
    }
}
