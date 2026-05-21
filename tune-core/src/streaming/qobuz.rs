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
            cover_url: album["image"]["large"].as_str().map(Into::into),
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
            cover_url: item["image"]["large"].as_str().map(Into::into),
            year: item["released_at"].as_u64().map(|ts| {
                1970 + (ts / 31_536_000) as u32
            }).or_else(|| item["release_date_original"].as_str().and_then(|d| d.get(..4)?.parse().ok())),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_url: item["image"]["large"].as_str().map(Into::into),
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for QobuzService {
    fn name(&self) -> &str {
        "qobuz"
    }

    fn enabled(&self) -> bool {
        !self.app_id.is_empty()
    }

    async fn authenticate(&mut self, credentials: &serde_json::Value) -> Result<AuthStatus, String> {
        let username = credentials["username"].as_str().ok_or("username required")?;
        let password = credentials["password"].as_str().ok_or("password required")?;

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
            info!(status, body = %body, app_id = %self.app_id, "qobuz_login_failed");
            return Err(format!("qobuz login {status}: {body}"));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        self.user_auth_token = data["user_auth_token"].as_str().map(Into::into);
        self.username = data["user"]["display_name"].as_str().map(Into::into);
        self.subscription = data["user"]["credential"]["label"].as_str().map(Into::into);

        info!(username = ?self.username, "qobuz_authenticated");
        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.user_auth_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            ..Default::default()
        }
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
            cover_url: data["image_rectangle_mini"].as_array()
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

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        let data = self.api_get("/playlist/getUserPlaylists", &[("limit", "500")]).await?;
        let playlists = data["playlists"]["items"].as_array()
            .map(|items| items.iter().map(|item| StreamPlaylist {
                id: item["id"].as_u64().unwrap_or(0).to_string(),
                name: item["name"].as_str().unwrap_or("").into(),
                description: item["description"].as_str().map(Into::into),
                cover_url: None,
                track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
                owner: None,
            }).collect())
            .unwrap_or_default();
        Ok(playlists)
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

    fn save_tokens(&self) -> Option<serde_json::Value> {
        let token = self.user_auth_token.as_ref()?;
        Some(serde_json::json!({
            "user_auth_token": token,
            "username": self.username,
            "subscription": self.subscription,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(t) = tokens["user_auth_token"].as_str() {
            self.user_auth_token = Some(t.into());
            self.username = tokens["username"].as_str().map(Into::into);
            self.subscription = tokens["subscription"].as_str().map(Into::into);
            true
        } else {
            false
        }
    }
}

fn md5_hex(input: &str) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}
