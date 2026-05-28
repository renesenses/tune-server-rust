use reqwest::Client;
use sha2::{Sha256, Digest};
use tracing::info;

use super::traits::*;

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const API_BASE: &str = "https://api.spotify.com/v1";
const DEFAULT_CLIENT_ID: &str = "placeholder";
const SCOPES: &str = "user-read-private user-library-read playlist-read-private";

pub struct SpotifyService {
    client: Client,
    client_id: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
    username: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: String,
    token_expires: Option<std::time::Instant>,
    enabled_override: Option<bool>,
}

impl Default for SpotifyService {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyService {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
            client_id: std::env::var("SPOTIFY_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.into()),
            access_token: None,
            refresh_token: None,
            username: None,
            code_verifier: None,
            redirect_uri: std::env::var("SPOTIFY_REDIRECT_URI")
                .unwrap_or_else(|_| "http://localhost:8085/api/v1/streaming/spotify/callback".into()),
            token_expires: None,
            enabled_override: None,
        }
    }

    fn generate_pkce() -> (String, String) {
        let verifier: String = (0..128)
            .map(|i| {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
                    .wrapping_add(i as u32);
                let idx = (seed % 62) as usize;
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"[idx] as char
            })
            .collect();

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let challenge = base64url_encode(&hash);

        (verifier, challenge)
    }

    fn auth_url(&self, challenge: &str) -> String {
        format!(
            "{AUTH_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge_method=S256&code_challenge={challenge}",
            urlencoding::encode(&self.client_id),
            urlencoding::encode(&self.redirect_uri),
            urlencoding::encode(SCOPES),
        )
    }

    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self.client
            .get(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("spotify api: {e}"))?;

        if resp.status() == 401 { return Err("token expired".into()); }
        resp.json().await.map_err(|e| format!("spotify json: {e}"))
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_str().unwrap_or("").into(),
            title: item["name"].as_str().unwrap_or("").into(),
            artist: item["artists"].as_array()
                .and_then(|a| a.first()).and_then(|a| a["name"].as_str()).unwrap_or("").into(),
            album: album["name"].as_str().map(Into::into),
            album_id: album["id"].as_str().map(Into::into),
            duration_ms: item["duration_ms"].as_u64().unwrap_or(0),
            cover_path: album["images"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into),
            track_number: item["track_number"].as_u64().map(|n| n as u32),
            disc_number: item["disc_number"].as_u64().map(|n| n as u32),
            explicit: item["explicit"].as_bool().unwrap_or(false),
            quality: Some(StreamQuality { codec: "OGG".into(), sample_rate: 44100, bit_depth: 16, bitrate: Some(320) }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"].as_str().unwrap_or("").into(),
            title: item["name"].as_str().unwrap_or("").into(),
            artist: item["artists"].as_array().and_then(|a| a.first()).and_then(|a| a["name"].as_str()).unwrap_or("").into(),
            artist_id: item["artists"].as_array().and_then(|a| a.first()).and_then(|a| a["id"].as_str()).map(Into::into),
            cover_path: item["images"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into),
            year: item["release_date"].as_str().and_then(|d| d.get(..4)?.parse().ok()),
            track_count: item["total_tracks"].as_u64().unwrap_or(0) as u32,
            quality: None,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["images"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into),
        }
    }

    fn map_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            description: item["description"].as_str().filter(|d| !d.is_empty()).map(Into::into),
            cover_path: item["images"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into),
            track_count: item["tracks"]["total"].as_u64().unwrap_or(0) as u32,
            owner: item["owner"]["display_name"].as_str().map(Into::into),
        }
    }

    fn map_category_to_genre(item: &serde_json::Value) -> StreamGenre {
        StreamGenre {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children: false,
            image_url: item["icons"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into),
        }
    }

    async fn api_put(&self, path: &str) -> Result<(), String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self.client
            .put(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("spotify put: {e}"))?;
        if resp.status() == 401 { return Err("token expired".into()); }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(format!("spotify PUT {path}: {status}"));
        }
        Ok(())
    }

    async fn api_delete(&self, path: &str) -> Result<(), String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self.client
            .delete(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("spotify delete: {e}"))?;
        if resp.status() == 401 { return Err("token expired".into()); }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(format!("spotify DELETE {path}: {status}"));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl StreamingService for SpotifyService {
    fn name(&self) -> &str { "spotify" }
    fn enabled(&self) -> bool { self.enabled_override.unwrap_or(self.client_id != DEFAULT_CLIENT_ID) }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(&mut self, credentials: &serde_json::Value) -> Result<AuthStatus, String> {
        if let Some(code) = credentials.get("code").and_then(|v| v.as_str()) {
            let verifier = self.code_verifier.take().ok_or("no code verifier")?;
            let resp = self.client.post(TOKEN_URL)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &self.redirect_uri),
                    ("client_id", &self.client_id),
                    ("code_verifier", &verifier),
                ])
                .send().await.map_err(|e| format!("token: {e}"))?;

            let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
            if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                return Err(format!("spotify: {err}"));
            }

            self.access_token = data["access_token"].as_str().map(Into::into);
            self.refresh_token = data["refresh_token"].as_str().map(Into::into);
            let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
            self.token_expires = Some(std::time::Instant::now() + std::time::Duration::from_secs(expires_in));
            let me = self.api_get("/me").await.ok();
            self.username = me.and_then(|v| v["display_name"].as_str().map(Into::into));
            info!(username = ?self.username, "spotify_authenticated");
            return Ok(self.auth_status().await);
        }

        let (verifier, challenge) = Self::generate_pkce();
        self.code_verifier = Some(verifier);
        Ok(AuthStatus {
            authenticated: false,
            verification_url: Some(self.auth_url(&challenge)),
            ..Default::default()
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus { authenticated: self.access_token.is_some(), username: self.username.clone(), ..Default::default() }
    }

    async fn logout(&mut self) -> Result<(), String> {
        self.access_token = None; self.refresh_token = None; self.username = None; Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String> {
        let data = self.api_get(&format!("/search?q={}&type=track,album,artist&limit={limit}", urlencoding::encode(query))).await?;
        Ok(SearchResults {
            tracks: data["tracks"]["items"].as_array().map(|i| i.iter().map(Self::map_track).collect()).unwrap_or_default(),
            albums: data["albums"]["items"].as_array().map(|i| i.iter().map(Self::map_album).collect()).unwrap_or_default(),
            artists: data["artists"]["items"].as_array().map(|i| i.iter().map(Self::map_artist).collect()).unwrap_or_default(),
            playlists: vec![],
        })
    }

    async fn get_track(&self, id: &str) -> Result<StreamTrack, String> { self.api_get(&format!("/tracks/{id}")).await.map(|d| Self::map_track(&d)) }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, String> { Err("Spotify requires Connect/librespot for streaming".into()) }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, String> { self.api_get(&format!("/albums/{id}")).await.map(|d| Self::map_album(&d)) }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, String> {
        self.api_get(&format!("/albums/{id}/tracks?limit=50")).await.map(|d| d["items"].as_array().map(|i| i.iter().map(Self::map_track).collect()).unwrap_or_default())
    }
    async fn get_artist(&self, id: &str) -> Result<StreamArtist, String> { self.api_get(&format!("/artists/{id}")).await.map(|d| Self::map_artist(&d)) }
    async fn get_playlist(&self, id: &str) -> Result<StreamPlaylist, String> {
        let d = self.api_get(&format!("/playlists/{id}")).await?;
        Ok(StreamPlaylist { id: d["id"].as_str().unwrap_or("").into(), name: d["name"].as_str().unwrap_or("").into(), description: d["description"].as_str().map(Into::into), cover_path: d["images"].as_array().and_then(|i| i.first()).and_then(|i| i["url"].as_str()).map(Into::into), track_count: d["tracks"]["total"].as_u64().unwrap_or(0) as u32, owner: d["owner"]["display_name"].as_str().map(Into::into) })
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, String> {
        self.api_get(&format!("/playlists/{id}/tracks?limit=100")).await.map(|d| d["items"].as_array().map(|i| i.iter().filter_map(|item| item.get("track").map(Self::map_track)).collect()).unwrap_or_default())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        self.api_get("/me/playlists?limit=50").await.map(|d| d["items"].as_array().map(|i| i.iter().map(Self::map_playlist).collect()).unwrap_or_default())
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get(&format!("/artists/{artist_id}/albums?include_groups=album,single&limit=50")).await?;
        Ok(data["items"].as_array().map(|i| i.iter().map(Self::map_album).collect()).unwrap_or_default())
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get(&format!("/artists/{artist_id}/top-tracks")).await?;
        Ok(data["tracks"].as_array().map(|i| i.iter().map(Self::map_track).collect()).unwrap_or_default())
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get("/browse/new-releases?limit=50").await?;
        Ok(data["albums"]["items"].as_array().map(|i| i.iter().map(Self::map_album).collect()).unwrap_or_default())
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, String> {
        let data = self.api_get("/browse/featured-playlists?limit=50").await?;
        Ok(data["playlists"]["items"].as_array().map(|i| i.iter().map(Self::map_playlist).collect()).unwrap_or_default())
    }

    async fn get_genres(&self) -> Result<Vec<StreamGenre>, String> {
        let data = self.api_get("/browse/categories?limit=50").await?;
        Ok(data["categories"]["items"].as_array().map(|i| i.iter().map(Self::map_category_to_genre).collect()).unwrap_or_default())
    }

    async fn get_genre_albums(&self, genre_id: &str, limit: usize) -> Result<Vec<StreamAlbum>, String> {
        let data = self.api_get(&format!("/browse/categories/{genre_id}/playlists?limit={limit}")).await?;
        let playlists: Vec<String> = data["playlists"]["items"].as_array()
            .map(|i| i.iter().filter_map(|p| p["id"].as_str().map(Into::into)).collect())
            .unwrap_or_default();
        let mut albums = Vec::new();
        for pid in playlists.iter().take(3) {
            if let Ok(tracks) = self.get_playlist_tracks(pid).await {
                for t in tracks {
                    if let (Some(album_id), Some(album_title)) = (&t.album_id, &t.album) {
                        if !albums.iter().any(|a: &StreamAlbum| a.id == *album_id) {
                            albums.push(StreamAlbum {
                                id: album_id.clone(),
                                title: album_title.clone(),
                                artist: t.artist.clone(),
                                artist_id: None,
                                cover_path: t.cover_path.clone(),
                                year: None,
                                track_count: 0,
                                quality: None,
                            });
                            if albums.len() >= limit { break; }
                        }
                    }
                }
                if albums.len() >= limit { break; }
            }
        }
        Ok(albums)
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, String> {
        let data = self.api_get("/me/tracks?limit=50").await?;
        Ok(data["items"].as_array().map(|i| i.iter().filter_map(|item| item.get("track").map(Self::map_track)).collect()).unwrap_or_default())
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let path = match fav_type {
            "tracks" => format!("/me/tracks?ids={item_id}"),
            "albums" => format!("/me/albums?ids={item_id}"),
            "artists" => format!("/me/following?type=artist&ids={item_id}"),
            _ => return Err(format!("unknown favorite type: {fav_type}")),
        };
        self.api_put(&path).await
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let path = match fav_type {
            "tracks" => format!("/me/tracks?ids={item_id}"),
            "albums" => format!("/me/albums?ids={item_id}"),
            "artists" => format!("/me/following?type=artist&ids={item_id}"),
            _ => return Err(format!("unknown favorite type: {fav_type}")),
        };
        self.api_delete(&path).await
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
        self.api_get("/me/albums?limit=50").await.map(|d| d["items"].as_array().map(|i| i.iter().filter_map(|item| item.get("album").map(Self::map_album)).collect()).unwrap_or_default())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        self.api_get("/me/following?type=artist&limit=50").await.map(|d| d["artists"]["items"].as_array().map(|i| i.iter().map(Self::map_artist).collect()).unwrap_or_default())
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        let needs_refresh = self
            .token_expires
            .map(|exp| {
                exp.checked_duration_since(std::time::Instant::now())
                    .map(|d| d.as_secs() < 300)
                    .unwrap_or(true)
            })
            .unwrap_or(false);

        if !needs_refresh {
            return Ok(false);
        }

        let refresh_token = self
            .refresh_token
            .as_ref()
            .ok_or("no refresh token")?
            .clone();

        let resp = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("refresh: {e}"))?;

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        if let Some(at) = data["access_token"].as_str() {
            self.access_token = Some(at.into());
            if let Some(rt) = data["refresh_token"].as_str() {
                self.refresh_token = Some(rt.into());
            }
            let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
            self.token_expires =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(expires_in));
            info!("spotify_token_refreshed");
            Ok(true)
        } else {
            Err("refresh failed".into())
        }
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        self.access_token.as_ref().map(|t| {
            serde_json::json!({
                "access_token": t,
                "refresh_token": self.refresh_token,
                "username": self.username,
            })
        })
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(at) = tokens["access_token"].as_str() {
            self.access_token = Some(at.into());
            self.refresh_token = tokens["refresh_token"].as_str().map(Into::into);
            self.username = tokens["username"].as_str().map(Into::into);
            self.token_expires =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
            true
        } else {
            false
        }
    }

    async fn post_restore(&mut self) {
        if let Ok(me) = self.api_get("/me").await {
            self.username = me["display_name"].as_str().map(Into::into);
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
            "id": "track123",
            "name": "Bohemian Rhapsody",
            "artists": [{"name": "Queen", "id": "artist1"}],
            "album": {
                "name": "A Night at the Opera",
                "id": "album1",
                "images": [{"url": "http://img.spotify.com/cover.jpg"}],
            },
            "duration_ms": 354000,
            "track_number": 11,
            "disc_number": 1,
            "explicit": false,
        });
        let track = SpotifyService::map_track(&json);
        assert_eq!(track.id, "track123");
        assert_eq!(track.title, "Bohemian Rhapsody");
        assert_eq!(track.artist, "Queen");
        assert_eq!(track.album.as_deref(), Some("A Night at the Opera"));
        assert_eq!(track.album_id.as_deref(), Some("album1"));
        assert_eq!(track.duration_ms, 354000);
        assert_eq!(track.track_number, Some(11));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(track.cover_path.as_deref(), Some("http://img.spotify.com/cover.jpg"));
        let q = track.quality.unwrap();
        assert_eq!(q.codec, "OGG");
        assert_eq!(q.bitrate, Some(320));
    }

    #[test]
    fn map_track_explicit() {
        let json = json!({
            "id": "x",
            "name": "Explicit",
            "artists": [{"name": "Test"}],
            "album": {},
            "duration_ms": 100000,
            "explicit": true,
        });
        let track = SpotifyService::map_track(&json);
        assert!(track.explicit);
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": null,
            "name": null,
            "album": {},
        });
        let track = SpotifyService::map_track(&json);
        assert_eq!(track.id, "");
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": "album123",
            "name": "A Night at the Opera",
            "artists": [{"name": "Queen", "id": "artist1"}],
            "images": [{"url": "http://img.spotify.com/album.jpg"}],
            "release_date": "1975-11-21",
            "total_tracks": 12,
        });
        let album = SpotifyService::map_album(&json);
        assert_eq!(album.id, "album123");
        assert_eq!(album.title, "A Night at the Opera");
        assert_eq!(album.artist, "Queen");
        assert_eq!(album.artist_id.as_deref(), Some("artist1"));
        assert_eq!(album.year, Some(1975));
        assert_eq!(album.track_count, 12);
        assert_eq!(album.cover_path.as_deref(), Some("http://img.spotify.com/album.jpg"));
    }

    #[test]
    fn map_album_year_only() {
        let json = json!({
            "id": "a",
            "name": "Test",
            "artists": [],
            "release_date": "2023",
            "total_tracks": 1,
        });
        let album = SpotifyService::map_album(&json);
        assert_eq!(album.year, Some(2023));
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": "artist123",
            "name": "Queen",
            "images": [{"url": "http://img.spotify.com/artist.jpg"}],
        });
        let artist = SpotifyService::map_artist(&json);
        assert_eq!(artist.id, "artist123");
        assert_eq!(artist.name, "Queen");
        assert_eq!(artist.image_path.as_deref(), Some("http://img.spotify.com/artist.jpg"));
    }

    #[test]
    fn map_artist_no_images() {
        let json = json!({
            "id": "a",
            "name": "Unknown",
            "images": [],
        });
        let artist = SpotifyService::map_artist(&json);
        assert!(artist.image_path.is_none());
    }

    #[test]
    fn spotify_service_name() {
        let svc = SpotifyService::new();
        assert_eq!(svc.name(), "spotify");
    }

    #[test]
    fn spotify_save_tokens_no_auth() {
        let svc = SpotifyService::new();
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn spotify_restore_tokens() {
        let mut svc = SpotifyService::new();
        let tokens = json!({
            "access_token": "spotify-token",
            "refresh_token": "refresh-token",
            "username": "testuser",
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.access_token.as_deref(), Some("spotify-token"));
        assert_eq!(svc.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
    }

    #[test]
    fn spotify_restore_tokens_invalid() {
        let mut svc = SpotifyService::new();
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    #[test]
    fn spotify_set_enabled() {
        let mut svc = SpotifyService::new();
        svc.set_enabled(true);
        assert!(svc.enabled());
        svc.set_enabled(false);
        assert!(!svc.enabled());
    }

    #[test]
    fn base64url_encode_known() {
        let data = b"Hello";
        let result = base64url_encode(data);
        // Standard base64 is "SGVsbG8=" but base64url has no padding and uses -_ instead of +/
        assert_eq!(result, "SGVsbG8");
    }
}

fn base64url_encode(data: &[u8]) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 6 { bits -= 6; output.push(table[((buf >> bits) & 0x3F) as usize] as char); }
    }
    if bits > 0 { buf <<= 6 - bits; output.push(table[(buf & 0x3F) as usize] as char); }
    output
}
