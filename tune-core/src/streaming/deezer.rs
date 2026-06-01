use std::time::Duration;

use reqwest::Client;
use tracing::info;

use super::traits::*;

const API_BASE: &str = "https://api.deezer.com";
const OAUTH_TOKEN_URL: &str = "https://connect.deezer.com/oauth/access_token.php";
const DEEZER_GW: &str = "https://www.deezer.com/ajax/gw-light.php";
const DEEZER_MEDIA_URL: &str = "https://media.deezer.com/v1";

pub struct DeezerService {
    client: Client,
    access_token: Option<String>,
    username: Option<String>,
    user_id: Option<u64>,
    enabled_override: Option<bool>,
    arl: Option<String>,
    license_token: Option<String>,
    api_token: Option<String>,
    quality: String,
    proxy_base_url: Option<String>,
}

impl Default for DeezerService {
    fn default() -> Self {
        Self::new()
    }
}

impl DeezerService {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .build()
                .unwrap(),
            access_token: None,
            username: None,
            user_id: None,
            enabled_override: None,
            arl: None,
            license_token: None,
            api_token: None,
            quality: "FLAC".into(),
            proxy_base_url: None,
        }
    }

    pub fn set_proxy_base_url(&mut self, url: Option<String>) {
        self.proxy_base_url = url;
    }

    pub fn has_full_streaming(&self) -> bool {
        self.arl.is_some() && self.license_token.is_some()
    }

    async fn gw_api_call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let arl = self.arl.as_ref().ok_or("deezer: no ARL")?;
        let url = format!(
            "{DEEZER_GW}?method={method}&input=3&api_version=1.0&api_token={}",
            self.api_token.as_deref().unwrap_or("")
        );
        let resp = self
            .client
            .post(&url)
            .header("Cookie", format!("arl={arl}"))
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "fr-FR,fr;q=0.9,en;q=0.8")
            .json(&params.unwrap_or(serde_json::json!({})))
            .send()
            .await
            .map_err(|e| format!("deezer gw: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("deezer gw body: {e}"))?;
        if !status.is_success() || body.starts_with('<') {
            tracing::warn!(method, %status, body_preview = &body[..body.len().min(200)], "deezer_gw_blocked");
            return Err(format!("deezer gw http {status}"));
        }
        let data: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("deezer gw json: {e}"))?;
        if let Some(err) = data.get("error").and_then(|e| e.as_object())
            && !err.is_empty()
        {
            return Err(format!("deezer gw error: {}", serde_json::json!(err)));
        }
        Ok(data.get("results").cloned().unwrap_or_default())
    }

    pub async fn authenticate_arl(&mut self, arl: &str) -> Result<bool, String> {
        if arl.len() < 100 {
            return Err("ARL trop court — doit faire ~192 caractères".into());
        }
        self.arl = Some(arl.into());
        let result = self.gw_api_call("deezer.getUserData", None).await?;
        let user = &result["USER"];
        let user_id_num = user["USER_ID"]
            .as_u64()
            .or_else(|| user["USER_ID"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        if user_id_num == 0 {
            self.arl = None;
            return Err("ARL invalide ou expiré".into());
        }
        self.user_id = Some(user_id_num);
        self.username = user["BLOG_NAME"].as_str().map(Into::into);
        self.license_token = user
            .get("OPTIONS")
            .and_then(|o| o.get("license_token"))
            .and_then(|v| v.as_str())
            .map(Into::into);
        if let Some(cf) = result.get("checkForm").and_then(|v| v.as_str()) {
            self.api_token = Some(cf.into());
        }
        info!(
            user_id = ?self.user_id,
            has_license = self.license_token.is_some(),
            "deezer_arl_authenticated"
        );
        Ok(true)
    }

    pub async fn get_full_stream_url(
        &self,
        track_id: &str,
        max_fallbacks: u8,
    ) -> Result<String, String> {
        let mut current_id = track_id.to_string();
        for attempt in 0..=max_fallbacks {
            let result = self
                .gw_api_call(
                    "song.getData",
                    Some(serde_json::json!({"SNG_ID": current_id})),
                )
                .await?;
            let token = result["TRACK_TOKEN"].as_str().ok_or("no TRACK_TOKEN")?;
            let license = self.license_token.as_ref().ok_or("no license_token")?;
            let format_name = match self.quality.as_str() {
                "FLAC" | "MP3_320" | "MP3_128" => self.quality.as_str(),
                _ => "FLAC",
            };
            let body = serde_json::json!({
                "license_token": license,
                "media": [{
                    "type": "FULL",
                    "formats": [{"cipher": "BF_CBC_STRIPE", "format": format_name}],
                }],
                "track_tokens": [token],
            });
            let resp = self
                .client
                .post(format!("{DEEZER_MEDIA_URL}/get_url"))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("deezer media: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("deezer media status: {}", resp.status()));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("deezer media json: {e}"))?;
            let entry = data["data"]
                .as_array()
                .and_then(|a| a.first())
                .unwrap_or(&serde_json::Value::Null);
            if let Some(url) = entry["media"]
                .as_array()
                .and_then(|m| m.first())
                .and_then(|m| m["sources"].as_array())
                .and_then(|s| s.first())
                .and_then(|s| s["url"].as_str())
            {
                info!(track_id = %current_id, quality = format_name, "deezer_stream_url_resolved");
                return Ok(url.to_string());
            }
            // Check geo-restriction → try fallback track
            let rights_denied = entry["errors"]
                .as_array()
                .map(|errs| errs.iter().any(|e| e["code"].as_u64() == Some(2002)))
                .unwrap_or(false);
            if rights_denied
                && attempt < max_fallbacks
                && let Some(fid) = result
                    .get("FALLBACK")
                    .and_then(|f| f.get("SNG_ID"))
                    .and_then(|v| {
                        v.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                && !fid.is_empty()
                && fid != current_id
            {
                info!(original = %current_id, fallback = %fid, "deezer_track_fallback");
                current_id = fid;
                continue;
            }
            break;
        }
        Err("no stream URL available".into())
    }

    /// Generic GET against the Deezer public API.
    /// Appends `access_token` query parameter when authenticated.
    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let mut url = format!("{API_BASE}{path}");
        if let Some(ref token) = self.access_token {
            let sep = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{sep}access_token={token}");
        }
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("deezer: {e}"))?;
        let data: serde_json::Value = resp.json().await.map_err(|e| format!("deezer json: {e}"))?;
        if let Some(err) = data.get("error")
            && err.is_object()
        {
            return Err(format!("deezer error: {err}"));
        }
        Ok(data)
    }

    /// Fetch the authenticated user profile and store username + user_id.
    async fn fetch_user_profile(&mut self) -> Result<(), String> {
        let data = self.api_get("/user/me").await?;
        self.username = data["name"].as_str().map(Into::into);
        self.user_id = data["id"].as_u64();
        Ok(())
    }

    // ── mapping helpers ──────────────────────────────────────────────

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str().unwrap_or("").into(),
            album: album["title"].as_str().map(Into::into),
            album_id: album["id"].as_u64().map(|id| id.to_string()),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_path: album["cover_big"]
                .as_str()
                .or_else(|| album["cover_medium"].as_str())
                .map(Into::into),
            track_number: item["track_position"].as_u64().map(|n| n as u32),
            disc_number: item["disk_number"].as_u64().map(|n| n as u32),
            explicit: item["explicit_lyrics"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality {
                codec: "MP3".into(),
                sample_rate: 44100,
                bit_depth: 16,
                bitrate: Some(128), // 30s preview is 128 kbps MP3
                channels: 2,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str().unwrap_or("").into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_path: item["cover_big"]
                .as_str()
                .or_else(|| item["cover_medium"].as_str())
                .map(Into::into),
            year: item["release_date"]
                .as_str()
                .and_then(|d| d.get(..4)?.parse().ok()),
            track_count: item["nb_tracks"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["picture_big"]
                .as_str()
                .or_else(|| item["picture_medium"].as_str())
                .map(Into::into),
        }
    }

    fn map_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["title"].as_str().unwrap_or("").into(),
            description: item["description"].as_str().map(Into::into),
            cover_path: item["picture_big"]
                .as_str()
                .or_else(|| item["picture_medium"].as_str())
                .map(Into::into),
            track_count: item["nb_tracks"].as_u64().unwrap_or(0) as u32,
            owner: item["creator"]["name"].as_str().map(Into::into),
        }
    }

    fn map_genre(item: &serde_json::Value) -> StreamGenre {
        StreamGenre {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children: false,
            image_url: item["picture_big"]
                .as_str()
                .or_else(|| item["picture_medium"].as_str())
                .map(Into::into),
        }
    }

    /// Collect items from a Deezer paginated `data` array.
    fn collect_data<T>(data: &serde_json::Value, mapper: fn(&serde_json::Value) -> T) -> Vec<T> {
        data["data"]
            .as_array()
            .map(|items| items.iter().map(mapper).collect())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl StreamingService for DeezerService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "deezer"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(true)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    // ── auth ─────────────────────────────────────────────────────────

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, String> {
        // Path 1: ARL token (full streaming with decrypt proxy)
        if let Some(arl) = credentials["arl"].as_str() {
            self.authenticate_arl(arl).await?;
            return Ok(self.auth_status().await);
        }

        // Path 2: pre-existing access token (testing / manual setup)
        if let Some(token) = credentials["access_token"].as_str() {
            self.access_token = Some(token.into());
            self.fetch_user_profile().await?;
            info!(username = ?self.username, "deezer_authenticated_token");
            return Ok(self.auth_status().await);
        }

        // Path 3: OAuth code exchange (server-side flow)
        let app_id = credentials["app_id"]
            .as_str()
            .ok_or("deezer: app_id required")?;
        let app_secret = credentials["app_secret"]
            .as_str()
            .ok_or("deezer: app_secret required")?;
        let code = credentials["code"]
            .as_str()
            .ok_or("deezer: code required")?;

        let url = format!(
            "{OAUTH_TOKEN_URL}?app_id={app_id}&secret={app_secret}&code={code}&output=json"
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("deezer oauth: {e}"))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("deezer oauth json: {e}"))?;

        if let Some(err) = data.get("error_reason").and_then(|v| v.as_str()) {
            return Err(format!("deezer oauth error: {err}"));
        }

        let token = data["access_token"]
            .as_str()
            .ok_or("deezer: no access_token in response")?;
        self.access_token = Some(token.into());
        self.fetch_user_profile().await?;
        info!(username = ?self.username, "deezer_authenticated_oauth");
        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.access_token.is_some() || self.arl.is_some(),
            username: self.username.clone(),
            subscription: None,
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), String> {
        self.access_token = None;
        self.username = None;
        self.user_id = None;
        Ok(())
    }

    // ── search ───────────────────────────────────────────────────────

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String> {
        let encoded = urlencoding::encode(query);
        let data = self
            .api_get(&format!("/search?q={encoded}&limit={limit}"))
            .await?;
        let tracks = Self::collect_data(&data, Self::map_track);

        // Deezer /search returns tracks by default.
        // Fetch albums and artists in parallel-ish for richer results.
        let albums_data = self
            .api_get(&format!("/search/album?q={encoded}&limit={limit}"))
            .await
            .unwrap_or_default();
        let artists_data = self
            .api_get(&format!("/search/artist?q={encoded}&limit={limit}"))
            .await
            .unwrap_or_default();
        let playlists_data = self
            .api_get(&format!("/search/playlist?q={encoded}&limit={limit}"))
            .await
            .unwrap_or_default();

        Ok(SearchResults {
            tracks,
            albums: Self::collect_data(&albums_data, Self::map_album),
            artists: Self::collect_data(&artists_data, Self::map_artist),
            playlists: Self::collect_data(&playlists_data, Self::map_playlist),
        })
    }

    // ── track ────────────────────────────────────────────────────────

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, String> {
        let data = self.api_get(&format!("/track/{track_id}")).await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, String> {
        // Path 1: local decrypt proxy (full quality, plain audio for DLNA)
        if let (Some(base), true) = (&self.proxy_base_url, self.has_full_streaming()) {
            let ext = if self.quality == "FLAC" {
                "flac"
            } else {
                "mp3"
            };
            let mime = if ext == "flac" {
                "audio/flac"
            } else {
                "audio/mpeg"
            };
            return Ok(StreamUrl {
                url: format!("{base}/deezer/{track_id}.{ext}"),
                mime_type: mime.into(),
                quality: StreamQuality {
                    codec: if ext == "flac" { "FLAC" } else { "MP3" }.into(),
                    sample_rate: 44100,
                    bit_depth: if ext == "flac" { 16 } else { 16 },
                    bitrate: if ext == "flac" { None } else { Some(320) },
                    channels: 2,
                },
                expires_at: None,
            });
        }

        // Path 2: direct encrypted URL (only for clients that decrypt)
        if self.has_full_streaming()
            && let Ok(url) = self.get_full_stream_url(track_id, 0).await
        {
            let ext = if self.quality == "FLAC" {
                "flac"
            } else {
                "mp3"
            };
            return Ok(StreamUrl {
                url,
                mime_type: if ext == "flac" {
                    "audio/flac"
                } else {
                    "audio/mpeg"
                }
                .into(),
                quality: StreamQuality {
                    codec: if ext == "flac" { "FLAC" } else { "MP3" }.into(),
                    sample_rate: 44100,
                    bit_depth: 16,
                    bitrate: if ext == "flac" { None } else { Some(320) },
                    channels: 2,
                },
                expires_at: None,
            });
        }

        // Fallback: 30s preview
        let data = self.api_get(&format!("/track/{track_id}")).await?;
        let preview = data["preview"]
            .as_str()
            .ok_or("deezer: no preview URL for this track")?;
        Ok(StreamUrl {
            url: preview.into(),
            mime_type: "audio/mpeg".into(),
            quality: StreamQuality {
                codec: "MP3".into(),
                sample_rate: 44100,
                bit_depth: 16,
                bitrate: Some(128),
                channels: 2,
            },
            expires_at: None,
        })
    }

    // ── album ────────────────────────────────────────────────────────

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, String> {
        let data = self.api_get(&format!("/album/{album_id}")).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get(&format!("/album/{album_id}/tracks")).await?;
        Ok(Self::collect_data(&data, Self::map_track))
    }

    // ── artist ───────────────────────────────────────────────────────

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, String> {
        let data = self.api_get(&format!("/artist/{artist_id}")).await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let data = self
            .api_get(&format!("/artist/{artist_id}/albums?limit=50"))
            .await?;
        Ok(Self::collect_data(&data, Self::map_album))
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self
            .api_get(&format!("/artist/{artist_id}/top?limit=20"))
            .await?;
        Ok(Self::collect_data(&data, Self::map_track))
    }

    // ── playlist ─────────────────────────────────────────────────────

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, String> {
        let data = self.api_get(&format!("/playlist/{playlist_id}")).await?;
        Ok(Self::map_playlist(&data))
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self
            .api_get(&format!("/playlist/{playlist_id}/tracks?limit=500"))
            .await?;
        Ok(Self::collect_data(&data, Self::map_track))
    }

    // ── user library (requires auth) ─────────────────────────────────

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let data = self.api_get("/user/me/playlists?limit=500").await?;
        Ok(Self::collect_data(&data, Self::map_playlist))
    }

    async fn create_playlist(
        &self,
        name: &str,
        _description: Option<&str>,
    ) -> Result<String, String> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("deezer: not authenticated")?;
        let user_id = self.user_id.ok_or("deezer: no user_id")?;
        let url = format!(
            "{API_BASE}/user/{user_id}/playlists?title={}&access_token={token}",
            urlencoding::encode(name)
        );
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| format!("deezer create_playlist: {e}"))?;
        let data: serde_json::Value = resp.json().await.map_err(|e| format!("deezer json: {e}"))?;
        data["id"]
            .as_u64()
            .map(|id| id.to_string())
            .ok_or_else(|| format!("deezer: no playlist id in response: {data}"))
    }

    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, String> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("deezer: not authenticated")?;
        let mut added = 0;
        for chunk in track_ids.chunks(100) {
            let songs = chunk.join(",");
            let url = format!(
                "{API_BASE}/playlist/{playlist_id}/tracks?songs={songs}&access_token={token}"
            );
            self.client
                .post(&url)
                .send()
                .await
                .map_err(|e| format!("deezer add_tracks: {e}"))?;
            added += chunk.len();
        }
        Ok(added)
    }

    fn supports_write(&self) -> bool {
        self.access_token.is_some() && self.user_id.is_some()
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let data = self.api_get("/user/me/albums?limit=500").await?;
        Ok(Self::collect_data(&data, Self::map_album))
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let data = self.api_get("/user/me/artists?limit=500").await?;
        Ok(Self::collect_data(&data, Self::map_artist))
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let data = self.api_get("/user/me/tracks?limit=500").await?;
        Ok(Self::collect_data(&data, Self::map_track))
    }

    // ── browse / editorial ───────────────────────────────────────────

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, String> {
        let data = self.api_get("/chart/0/playlists?limit=50").await?;
        Ok(Self::collect_data(&data, Self::map_playlist))
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get("/chart/0/albums?limit=50").await?;
        Ok(Self::collect_data(&data, Self::map_album))
    }

    async fn get_genres(&self) -> Result<Vec<StreamGenre>, String> {
        let data = self.api_get("/genre").await?;
        Ok(Self::collect_data(&data, Self::map_genre))
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, String> {
        // Deezer editorial endpoint gives albums for a genre
        let data = self
            .api_get(&format!("/editorial/{genre_id}/releases?limit={limit}"))
            .await;
        match data {
            Ok(d) => Ok(Self::collect_data(&d, Self::map_album)),
            Err(_) => {
                // Fallback: get genre artists then their albums
                let artists_data = self.api_get(&format!("/genre/{genre_id}/artists")).await?;
                let artists = Self::collect_data(&artists_data, Self::map_artist);
                let mut albums = Vec::new();
                for artist in artists.iter().take(5) {
                    if let Ok(artist_albums) = self.get_artist_albums(&artist.id).await {
                        albums.extend(artist_albums);
                        if albums.len() >= limit {
                            break;
                        }
                    }
                }
                albums.truncate(limit);
                Ok(albums)
            }
        }
    }

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, String> {
        Ok(vec![
            FeaturedSection {
                id: "charts".into(),
                name: "Charts".into(),
            },
            FeaturedSection {
                id: "new-releases".into(),
                name: "New Releases".into(),
            },
        ])
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, String> {
        match section_id {
            "charts" => {
                let data = self.api_get("/chart/0/albums?limit=50").await?;
                Ok(Self::collect_data(&data, Self::map_album))
            }
            "new-releases" => self.get_new_releases().await,
            _ => Ok(vec![]),
        }
    }

    // ── favorites ────────────────────────────────────────────────────

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let endpoint = match fav_type {
            "tracks" => format!("/user/me/tracks?track_id={item_id}"),
            "albums" => format!("/user/me/albums?album_id={item_id}"),
            "artists" => format!("/user/me/artists?artist_id={item_id}"),
            "playlists" => format!("/user/me/playlists?playlist_id={item_id}"),
            _ => return Err(format!("deezer: unknown favorite type: {fav_type}")),
        };

        let mut url = format!("{API_BASE}{endpoint}");
        if let Some(ref token) = self.access_token {
            url = format!("{url}&access_token={token}");
        }
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| format!("deezer add_favorite: {e}"))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("deezer add_favorite: {body}"));
        }
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        self.access_token
            .as_ref()
            .ok_or_else(|| "deezer: not authenticated".to_string())?;
        let endpoint = match fav_type {
            "tracks" => format!("/user/me/tracks?track_id={item_id}"),
            "albums" => format!("/user/me/albums?album_id={item_id}"),
            "artists" => format!("/user/me/artists?artist_id={item_id}"),
            "playlists" => format!("/user/me/playlists?playlist_id={item_id}"),
            _ => return Err(format!("deezer: unknown favorite type: {fav_type}")),
        };

        let mut url = format!("{API_BASE}{endpoint}");
        if let Some(ref token) = self.access_token {
            url = format!("{url}&access_token={token}");
        }
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| format!("deezer remove_favorite: {e}"))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("deezer remove_favorite: {body}"));
        }
        Ok(())
    }

    // ── token persistence ────────────────────────────────────────────

    fn save_tokens(&self) -> Option<serde_json::Value> {
        if self.access_token.is_none() && self.arl.is_none() {
            return None;
        }
        Some(serde_json::json!({
            "access_token": self.access_token,
            "username": self.username,
            "user_id": self.user_id,
            "arl": self.arl,
            "quality": self.quality,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        let mut restored = false;
        if let Some(t) = tokens["access_token"].as_str() {
            self.access_token = Some(t.into());
            restored = true;
        }
        if let Some(arl) = tokens["arl"].as_str() {
            self.arl = Some(arl.into());
            restored = true;
        }
        if let Some(q) = tokens["quality"].as_str() {
            self.quality = q.into();
        }
        self.username = tokens["username"].as_str().map(Into::into);
        self.user_id = tokens["user_id"].as_u64();
        restored
    }

    async fn post_restore(&mut self) {
        // Re-authenticate ARL to get license_token
        if let Some(arl) = self.arl.clone() {
            match self.authenticate_arl(&arl).await {
                Ok(_) => info!(username = ?self.username, "deezer_arl_restored"),
                Err(e) => {
                    info!(error = %e, "deezer_arl_invalid_after_restore");
                    self.arl = None;
                    self.license_token = None;
                }
            }
        }
        // Validate OAuth token if present
        if self.access_token.is_some() {
            if let Err(e) = self.fetch_user_profile().await {
                info!(error = %e, "deezer_token_invalid_after_restore");
                self.access_token = None;
                self.username = None;
                self.user_id = None;
            } else {
                info!(username = ?self.username, "deezer_token_restored");
            }
        }
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        // Deezer tokens don't expire unless revoked, but validate if present
        if self.access_token.is_none() {
            return Ok(false);
        }
        match self.api_get("/user/me").await {
            Ok(_) => Ok(false),
            Err(e) => {
                info!(error = %e, "deezer_token_expired");
                self.access_token = None;
                self.username = None;
                self.user_id = None;
                Err("deezer: token expired or revoked".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_track_basic() {
        let json = json!({
            "id": 12345,
            "title": "Get Lucky",
            "artist": {"name": "Daft Punk"},
            "album": {
                "title": "Random Access Memories",
                "id": 678,
                "cover_big": "https://img.deezer.com/cover_big.jpg",
            },
            "duration": 369,
            "track_position": 8,
            "disk_number": 1,
            "explicit_lyrics": false,
        });
        let track = DeezerService::map_track(&json);
        assert_eq!(track.id, "12345");
        assert_eq!(track.title, "Get Lucky");
        assert_eq!(track.artist, "Daft Punk");
        assert_eq!(track.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(track.album_id.as_deref(), Some("678"));
        assert_eq!(track.duration_ms, 369_000);
        assert_eq!(track.track_number, Some(8));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(
            track.cover_path.as_deref(),
            Some("https://img.deezer.com/cover_big.jpg")
        );
    }

    #[test]
    fn map_track_explicit() {
        let json = json!({
            "id": 1,
            "title": "Explicit Track",
            "artist": {"name": "Test"},
            "album": {},
            "duration": 200,
            "explicit_lyrics": true,
        });
        let track = DeezerService::map_track(&json);
        assert!(track.explicit);
    }

    #[test]
    fn map_track_cover_fallback() {
        let json = json!({
            "id": 1,
            "title": "Test",
            "artist": {"name": "Test"},
            "album": {
                "cover_medium": "http://medium.jpg",
            },
            "duration": 100,
        });
        let track = DeezerService::map_track(&json);
        assert_eq!(track.cover_path.as_deref(), Some("http://medium.jpg"));
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": 0,
            "album": {},
        });
        let track = DeezerService::map_track(&json);
        assert_eq!(track.id, "0");
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
        let q = track.quality.unwrap();
        assert_eq!(q.codec, "MP3");
        assert_eq!(q.bitrate, Some(128));
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": 999,
            "title": "Random Access Memories",
            "artist": {"name": "Daft Punk", "id": 42},
            "cover_big": "http://cover.jpg",
            "release_date": "2013-05-17",
            "nb_tracks": 13,
        });
        let album = DeezerService::map_album(&json);
        assert_eq!(album.id, "999");
        assert_eq!(album.title, "Random Access Memories");
        assert_eq!(album.artist, "Daft Punk");
        assert_eq!(album.artist_id.as_deref(), Some("42"));
        assert_eq!(album.year, Some(2013));
        assert_eq!(album.track_count, 13);
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": 42,
            "name": "Daft Punk",
            "picture_big": "http://pic.jpg",
        });
        let artist = DeezerService::map_artist(&json);
        assert_eq!(artist.id, "42");
        assert_eq!(artist.name, "Daft Punk");
        assert_eq!(artist.image_path.as_deref(), Some("http://pic.jpg"));
    }

    #[test]
    fn map_artist_picture_fallback() {
        let json = json!({
            "id": 1,
            "name": "Test",
            "picture_medium": "http://medium.jpg",
        });
        let artist = DeezerService::map_artist(&json);
        assert_eq!(artist.image_path.as_deref(), Some("http://medium.jpg"));
    }

    #[test]
    fn map_playlist_basic() {
        let json = json!({
            "id": 555,
            "title": "My Playlist",
            "description": "Best songs",
            "picture_big": "http://pic.jpg",
            "nb_tracks": 25,
            "creator": {"name": "User1"},
        });
        let pl = DeezerService::map_playlist(&json);
        assert_eq!(pl.id, "555");
        assert_eq!(pl.name, "My Playlist");
        assert_eq!(pl.description.as_deref(), Some("Best songs"));
        assert_eq!(pl.track_count, 25);
        assert_eq!(pl.owner.as_deref(), Some("User1"));
    }

    #[test]
    fn map_genre_basic() {
        let json = json!({
            "id": 10,
            "name": "Pop",
            "picture_big": "http://genre.jpg",
        });
        let genre = DeezerService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Pop");
        assert!(!genre.has_children);
        assert_eq!(genre.image_url.as_deref(), Some("http://genre.jpg"));
    }

    #[test]
    fn collect_data_tracks() {
        let data = json!({
            "data": [
                {"id": 1, "title": "A", "artist": {"name": "X"}, "album": {}, "duration": 100},
                {"id": 2, "title": "B", "artist": {"name": "Y"}, "album": {}, "duration": 200},
            ]
        });
        let tracks = DeezerService::collect_data(&data, DeezerService::map_track);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "A");
        assert_eq!(tracks[1].title, "B");
    }

    #[test]
    fn collect_data_empty() {
        let data = json!({});
        let tracks = DeezerService::collect_data(&data, DeezerService::map_track);
        assert!(tracks.is_empty());
    }

    #[test]
    fn deezer_service_name() {
        let svc = DeezerService::new();
        assert_eq!(svc.name(), "deezer");
        assert!(svc.enabled());
    }

    #[test]
    fn deezer_set_enabled() {
        let mut svc = DeezerService::new();
        svc.set_enabled(false);
        assert!(!svc.enabled());
    }

    #[test]
    fn deezer_save_tokens_no_auth() {
        let svc = DeezerService::new();
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn deezer_restore_tokens() {
        let mut svc = DeezerService::new();
        let tokens = json!({
            "access_token": "deezer-token-123",
            "username": "testuser",
            "user_id": 999,
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.access_token.as_deref(), Some("deezer-token-123"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
        assert_eq!(svc.user_id, Some(999));
    }

    #[test]
    fn deezer_restore_tokens_invalid() {
        let mut svc = DeezerService::new();
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    #[test]
    fn deezer_supports_write() {
        let mut svc = DeezerService::new();
        assert!(!svc.supports_write());
        svc.access_token = Some("token".into());
        assert!(!svc.supports_write()); // need user_id too
        svc.user_id = Some(12345);
        assert!(svc.supports_write());
    }
}
