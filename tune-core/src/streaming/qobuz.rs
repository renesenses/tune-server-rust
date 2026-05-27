use std::time::Duration;

use reqwest::Client;
use tracing::info;

use super::traits::*;

const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";
const API_PROXY: &str = "https://mozaiklabs.fr/qobuz-api";
const REMOTE_CONFIG_URL: &str = "https://mozaiklabs.fr/storage/api/v1/streaming-config.json";

pub struct QobuzService {
    client: Client,
    app_id: String,
    app_secret: String,
    user_auth_token: Option<String>,
    username: Option<String>,
    subscription: Option<String>,
    use_proxy: bool,
    stored_username: Option<String>,
    stored_password: Option<String>,
    enabled_override: Option<bool>,
}

impl QobuzService {
    pub fn new(app_id: String, app_secret: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
                .build()
                .unwrap(),
            app_id,
            app_secret,
            user_auth_token: None,
            username: None,
            subscription: None,
            use_proxy: true,
            stored_username: None,
            stored_password: None,
            enabled_override: None,
        }
    }

    fn api_base(&self) -> &str {
        if self.use_proxy { API_PROXY } else { API_BASE }
    }

    async fn refresh_credentials(&mut self) {
        match self.client.get(REMOTE_CONFIG_URL).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let qobuz = &data["qobuz"];
                    if let (Some(id), Some(secret)) = (qobuz["app_id"].as_str(), qobuz["app_secret"].as_str()) {
                        info!(old_id = %&self.app_id, new_id = %id, "qobuz_credentials_refreshed");
                        self.app_id = id.to_string();
                        self.app_secret = secret.to_string();
                    }
                }
            }
            _ => info!("qobuz_remote_config_unavailable"),
        }
    }

    async fn api_get(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
        let base = self.api_base();
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("app_id", app_id));

        let mut req = self.client.get(&url).query(&query)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req.send().await.map_err(|e| format!("qobuz api: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            info!(path, status, body = %body, "qobuz_api_error");
            return Err(format!("qobuz {path}: {status} {body}"));
        }

        resp.json().await.map_err(|e| format!("qobuz json: {e}"))
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["performer"]["name"].as_str()
                .or_else(|| item["artist"]["name"].as_str())
                .unwrap_or("").into(),
            album: album["title"].as_str().map(Into::into),
            album_id: album["id"].as_str().map(Into::into)
                .or_else(|| album["id"].as_u64().map(|id| id.to_string())),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_path: album["image"]["large"].as_str().map(Into::into),
            track_number: item["track_number"].as_u64().map(|n| n as u32),
            disc_number: item["media_number"].as_u64().map(|n| n as u32),
            explicit: item["parental_warning"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate: item["maximum_sampling_rate"].as_f64().map(|r| (r * 1000.0) as u32).unwrap_or(44100),
                bit_depth: item["maximum_bit_depth"].as_u64().map(|b| b as u16).unwrap_or(16),
                bitrate: None,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"].as_str().map(Into::into)
                .or_else(|| item["id"].as_u64().map(|id| id.to_string()))
                .unwrap_or_default(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str().unwrap_or("").into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_path: item["image"]["large"].as_str().map(Into::into),
            year: item["released_at"].as_u64().map(|ts| {
                1970 + (ts / 31_536_000) as u32
            }).or_else(|| item["release_date_original"].as_str().and_then(|d| d.get(..4)?.parse().ok())),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    async fn login_internal(&mut self, username: &str, password: &str) -> Result<AuthStatus, String> {
        self.refresh_credentials().await;

        let base = self.api_base();
        let resp = self.client
            .post(format!("{base}/user/login"))
            .query(&[("app_id", self.app_id.as_str())])
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|e| format!("qobuz login: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            info!(status, body = %body, "qobuz_login_failed");
            return Err(format!("qobuz login {status}: {body}"));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        self.user_auth_token = data["user_auth_token"].as_str().map(Into::into);
        self.username = data["user"]["display_name"].as_str().map(Into::into);
        self.subscription = data["user"]["credential"]["label"].as_str().map(Into::into);

        info!(username = ?self.username, "qobuz_authenticated");
        Ok(self.auth_status_internal())
    }

    fn auth_status_internal(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.user_auth_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            ..Default::default()
        }
    }

    async fn auto_relogin(&mut self) -> bool {
        if let (Some(u), Some(p)) = (self.stored_username.clone(), self.stored_password.clone()) {
            info!("qobuz_auto_relogin");
            self.login_internal(&u, &p).await.is_ok()
        } else {
            false
        }
    }

    async fn api_post(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
        let base = self.api_base();
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("app_id", app_id));

        let mut req = self.client.post(&url).query(&query)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req.send().await.map_err(|e| format!("qobuz post: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qobuz {path}: {status} {body}"));
        }
        resp.json().await.or_else(|_| Ok(serde_json::json!({"ok": true})))
    }

    fn map_genre(item: &serde_json::Value) -> StreamGenre {
        StreamGenre {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children: item["subgenres"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
            image_url: item["image"].as_str().map(Into::into),
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["image"]["large"].as_str().map(Into::into),
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for QobuzService {
    fn name(&self) -> &str {
        "qobuz"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(!self.app_id.is_empty())
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(&mut self, credentials: &serde_json::Value) -> Result<AuthStatus, String> {
        let username = credentials["username"].as_str().ok_or("username required")?;
        let password = credentials["password"].as_str().ok_or("password required")?;

        self.stored_username = Some(username.to_string());
        self.stored_password = Some(password.to_string());

        self.login_internal(username, password).await
    }

    async fn auth_status(&self) -> AuthStatus {
        self.auth_status_internal()
    }

    async fn logout(&mut self) -> Result<(), String> {
        self.user_auth_token = None;
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String> {
        let data = self.api_get("/catalog/search", &[
            ("query", query),
            ("limit", &limit.to_string()),
        ]).await?;

        let tracks = data["tracks"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        let artists = data["artists"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();

        Ok(SearchResults { tracks, albums, artists, playlists: vec![] })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, String> {
        let data = self.api_get("/track/get", &[("track_id", track_id)]).await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(&self, track_id: &str, quality: Option<&str>) -> Result<StreamUrl, String> {
        let format_id = match quality {
            Some("hires") => "27",
            Some("cd") => "6",
            _ => "27",
        };

        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let timestamp = format!("{}.{}", dur.as_secs(), dur.subsec_millis());

        let sig_input = format!("trackgetFileUrlformat_id{format_id}intentstreamtrack_id{track_id}{timestamp}{}", self.app_secret);
        let sig = md5_hex(&sig_input);

        info!(track_id, format_id, timestamp = %timestamp, sig = %sig, "qobuz_get_file_url");

        let data = self.api_get("/track/getFileUrl", &[
            ("track_id", track_id),
            ("format_id", format_id),
            ("intent", "stream"),
            ("request_ts", &timestamp),
            ("request_sig", &sig),
        ]).await?;

        let url = data["url"].as_str().ok_or("no url")?.to_string();
        let mime = data["mime_type"].as_str().unwrap_or("audio/flac").to_string();
        let sample_rate = data["sampling_rate"].as_f64().map(|r| (r * 1000.0) as u32).unwrap_or(44100);
        let bit_depth = data["bit_depth"].as_u64().map(|b| b as u16).unwrap_or(16);

        Ok(StreamUrl {
            url,
            mime_type: mime,
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
        let data = self.api_get("/album/get", &[("album_id", album_id)]).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get("/album/get", &[("album_id", album_id)]).await?;
        let tracks = data["tracks"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, String> {
        let data = self.api_get("/artist/get", &[("artist_id", artist_id)]).await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, String> {
        let data = self.api_get("/playlist/get", &[("playlist_id", playlist_id)]).await?;
        Ok(StreamPlaylist {
            id: data["id"].as_u64().unwrap_or(0).to_string(),
            name: data["name"].as_str().unwrap_or("").into(),
            description: data["description"].as_str().map(Into::into),
            cover_path: data["image_rectangle_mini"].as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(Into::into),
            track_count: data["tracks_count"].as_u64().unwrap_or(0) as u32,
            owner: data["owner"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get("/playlist/get", &[
            ("playlist_id", playlist_id),
            ("extra", "tracks"),
            ("limit", "500"),
        ]).await?;
        let tracks = data["tracks"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_genres(&self) -> Result<Vec<StreamGenre>, String> {
        let data = self.api_get("/genre/list", &[]).await?;
        let genres = data["genres"]["items"].as_array()
            .or_else(|| data["genres"].as_array())
            .or_else(|| data.as_array())
            .map(|items| items.iter().map(Self::map_genre).collect())
            .unwrap_or_default();
        Ok(genres)
    }

    async fn get_genre_albums(&self, genre_id: &str, limit: usize) -> Result<Vec<StreamAlbum>, String> {
        let limit_str = limit.to_string();
        let data = self.api_get("/genre/get", &[
            ("genre_id", genre_id),
            ("type", "albums"),
            ("limit", &limit_str),
        ]).await?;
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, String> {
        Ok(vec![
            FeaturedSection { id: "new-releases".into(), name: "New Releases".into() },
            FeaturedSection { id: "best-sellers".into(), name: "Best Sellers".into() },
            FeaturedSection { id: "press-awards".into(), name: "Press Awards".into() },
            FeaturedSection { id: "editor-picks".into(), name: "Editor Picks".into() },
        ])
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get("/album/getFeatured", &[
            ("type", section_id),
            ("limit", "50"),
        ]).await?;
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get("/favorite/getUserFavorites", &[("type", "tracks"), ("limit", "500")]).await?;
        let tracks = data["tracks"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let key = match fav_type {
            "tracks" => "track_ids",
            "albums" => "album_ids",
            "artists" => "artist_ids",
            _ => return Err(format!("unknown favorite type: {fav_type}")),
        };
        self.api_post("/favorite/create", &[(key, item_id)]).await?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let key = match fav_type {
            "tracks" => "track_ids",
            "albums" => "album_ids",
            "artists" => "artist_ids",
            _ => return Err(format!("unknown favorite type: {fav_type}")),
        };
        self.api_post("/favorite/delete", &[(key, item_id)]).await?;
        Ok(())
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        let data = self.api_get("/playlist/getUserPlaylists", &[("limit", "500")]).await?;
        let playlists = data["playlists"]["items"].as_array()
            .map(|items| items.iter().map(|item| StreamPlaylist {
                id: item["id"].as_u64().unwrap_or(0).to_string(),
                name: item["name"].as_str().unwrap_or("").into(),
                description: item["description"].as_str().map(Into::into),
                cover_path: None,
                track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
                owner: None,
            }).collect())
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get("/artist/get", &[
            ("artist_id", artist_id),
            ("extra", "albums"),
            ("limit", "50"),
        ]).await?;
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get("/artist/get", &[
            ("artist_id", artist_id),
            ("extra", "tracks_appears_on"),
            ("limit", "20"),
        ]).await?;
        let tracks = data["tracks_appears_on"]["items"].as_array()
            .or_else(|| data["tracks"]["items"].as_array())
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get("/favorite/getUserFavorites", &[("type", "albums"), ("limit", "500")]).await?;
        let albums = data["albums"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        let data = self.api_get("/favorite/getUserFavorites", &[("type", "artists"), ("limit", "500")]).await?;
        let artists = data["artists"]["items"].as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();
        Ok(artists)
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        if self.user_auth_token.is_none() {
            return Ok(false);
        }
        let test = self.api_get("/user/get", &[]).await;
        if let Err(ref e) = test {
            if e.contains("401") || e.contains("403") {
                if self.auto_relogin().await {
                    info!("qobuz_token_refreshed_via_relogin");
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        let token = self.user_auth_token.as_ref()?;
        Some(serde_json::json!({
            "user_auth_token": token,
            "username": self.username,
            "subscription": self.subscription,
            "app_id": self.app_id,
            "app_secret": self.app_secret,
            "stored_username": self.stored_username,
            "stored_password": self.stored_password,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(t) = tokens["user_auth_token"].as_str() {
            self.user_auth_token = Some(t.into());
            self.username = tokens["username"].as_str().map(Into::into);
            self.subscription = tokens["subscription"].as_str().map(Into::into);
            if let Some(id) = tokens["app_id"].as_str() {
                self.app_id = id.into();
            }
            if let Some(secret) = tokens["app_secret"].as_str() {
                self.app_secret = secret.into();
            }
            self.stored_username = tokens["stored_username"].as_str().map(Into::into);
            self.stored_password = tokens["stored_password"].as_str().map(Into::into);
            true
        } else {
            false
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
            "title": "Take Five",
            "performer": {"name": "Dave Brubeck"},
            "album": {
                "title": "Time Out",
                "id": 678,
                "image": {"large": "http://img.qobuz.com/large.jpg"},
            },
            "duration": 324,
            "track_number": 2,
            "media_number": 1,
            "parental_warning": false,
            "maximum_sampling_rate": 192.0,
            "maximum_bit_depth": 24,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.id, "12345");
        assert_eq!(track.title, "Take Five");
        assert_eq!(track.artist, "Dave Brubeck");
        assert_eq!(track.album.as_deref(), Some("Time Out"));
        assert_eq!(track.album_id.as_deref(), Some("678"));
        assert_eq!(track.duration_ms, 324_000);
        assert_eq!(track.track_number, Some(2));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(track.cover_path.as_deref(), Some("http://img.qobuz.com/large.jpg"));
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 192000);
        assert_eq!(q.bit_depth, 24);
    }

    #[test]
    fn map_track_artist_fallback() {
        let json = json!({
            "id": 1,
            "title": "Test",
            "artist": {"name": "Fallback Artist"},
            "album": {},
            "duration": 100,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Fallback Artist");
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": 0,
            "title": null,
            "album": {},
            "duration": null,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 44100);
        assert_eq!(q.bit_depth, 16);
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": 999,
            "title": "Time Out",
            "artist": {"name": "Dave Brubeck", "id": 42},
            "image": {"large": "http://img.qobuz.com/album.jpg"},
            "release_date_original": "1959-12-14",
            "tracks_count": 7,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "999");
        assert_eq!(album.title, "Time Out");
        assert_eq!(album.artist, "Dave Brubeck");
        assert_eq!(album.artist_id.as_deref(), Some("42"));
        assert_eq!(album.year, Some(1959));
        assert_eq!(album.track_count, 7);
        assert_eq!(album.cover_path.as_deref(), Some("http://img.qobuz.com/album.jpg"));
    }

    #[test]
    fn map_album_with_released_at_timestamp() {
        let json = json!({
            "id": "abc",
            "title": "Test",
            "artist": {"name": "Test"},
            "released_at": 1580515200, // ~2020
            "tracks_count": 10,
        });
        let album = QobuzService::map_album(&json);
        assert!(album.year.is_some());
        assert!(album.year.unwrap() >= 2019 && album.year.unwrap() <= 2021);
    }

    #[test]
    fn map_album_string_id() {
        let json = json!({
            "id": "abc123",
            "title": "Test",
            "artist": {},
            "tracks_count": 0,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "abc123");
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": 42,
            "name": "Dave Brubeck",
            "image": {"large": "http://img.qobuz.com/artist.jpg"},
        });
        let artist = QobuzService::map_artist(&json);
        assert_eq!(artist.id, "42");
        assert_eq!(artist.name, "Dave Brubeck");
        assert_eq!(artist.image_path.as_deref(), Some("http://img.qobuz.com/artist.jpg"));
    }

    #[test]
    fn map_genre_basic() {
        let json = json!({
            "id": 10,
            "name": "Jazz",
            "subgenres": [{"id": 11, "name": "Bebop"}],
            "image": "http://img.qobuz.com/jazz.jpg",
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
    }

    #[test]
    fn map_genre_no_subgenres() {
        let json = json!({
            "id": 20,
            "name": "Blues",
            "subgenres": [],
        });
        let genre = QobuzService::map_genre(&json);
        assert!(!genre.has_children);
    }

    #[test]
    fn qobuz_service_name() {
        let svc = QobuzService::new("app123".into(), "secret".into());
        assert_eq!(svc.name(), "qobuz");
    }

    #[test]
    fn qobuz_save_tokens_no_auth() {
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn qobuz_restore_tokens() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "username": "testuser",
            "subscription": "Studio",
            "app_id": "new_app",
            "app_secret": "new_secret",
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.user_auth_token.as_deref(), Some("token123"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
        assert_eq!(svc.app_id, "new_app");
        assert_eq!(svc.app_secret, "new_secret");
    }

    #[test]
    fn qobuz_restore_tokens_invalid() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    #[test]
    fn qobuz_set_enabled() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.set_enabled(false);
        assert!(!svc.enabled());
        svc.set_enabled(true);
        assert!(svc.enabled());
    }

    #[test]
    fn md5_hex_known_value() {
        // MD5 of empty string is d41d8cd98f00b204e9800998ecf8427e
        let result = md5_hex("");
        assert_eq!(result, "d41d8cd98f00b204e9800998ecf8427e");
    }
}

fn md5_hex(input: &str) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}
