use std::time::Duration;

use reqwest::Client;
use tracing::info;

use super::traits::*;

const API_BASE: &str = "https://api.deezer.com";
const OAUTH_TOKEN_URL: &str = "https://connect.deezer.com/oauth/access_token.php";

pub struct DeezerService {
    client: Client,
    access_token: Option<String>,
    username: Option<String>,
    user_id: Option<u64>,
    enabled_override: Option<bool>,
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
                .build()
                .unwrap(),
            access_token: None,
            username: None,
            user_id: None,
            enabled_override: None,
        }
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
        if let Some(err) = data.get("error") {
            if err.is_object() {
                return Err(format!("deezer error: {err}"));
            }
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
            artist: item["artist"]["name"]
                .as_str()
                .unwrap_or("")
                .into(),
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
        // Path 1: pre-existing access token (testing / manual setup)
        if let Some(token) = credentials["access_token"].as_str() {
            self.access_token = Some(token.into());
            self.fetch_user_profile().await?;
            info!(username = ?self.username, "deezer_authenticated_token");
            return Ok(self.auth_status().await);
        }

        // Path 2: OAuth code exchange (server-side flow)
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
            authenticated: self.access_token.is_some(),
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
        let data = self
            .api_get(&format!("/album/{album_id}/tracks"))
            .await?;
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
        let data = self
            .api_get(&format!("/playlist/{playlist_id}"))
            .await?;
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

    async fn get_genre_albums(&self, genre_id: &str, limit: usize) -> Result<Vec<StreamAlbum>, String> {
        // Deezer editorial endpoint gives albums for a genre
        let data = self
            .api_get(&format!("/editorial/{genre_id}/releases?limit={limit}"))
            .await;
        match data {
            Ok(d) => Ok(Self::collect_data(&d, Self::map_album)),
            Err(_) => {
                // Fallback: get genre artists then their albums
                let artists_data = self
                    .api_get(&format!("/genre/{genre_id}/artists"))
                    .await?;
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
        self.access_token.as_ref().map(|t| {
            serde_json::json!({
                "access_token": t,
                "username": self.username,
                "user_id": self.user_id,
            })
        })
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(t) = tokens["access_token"].as_str() {
            self.access_token = Some(t.into());
            self.username = tokens["username"].as_str().map(Into::into);
            self.user_id = tokens["user_id"].as_u64();
            true
        } else {
            false
        }
    }

    async fn post_restore(&mut self) {
        // Validate the restored token by fetching the user profile
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
