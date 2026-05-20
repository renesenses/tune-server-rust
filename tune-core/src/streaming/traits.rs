use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    pub cover_url: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub explicit: bool,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub cover_url: Option<String>,
    pub year: Option<u32>,
    pub track_count: u32,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtist {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlaylist {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cover_url: Option<String>,
    pub track_count: u32,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamQuality {
    pub codec: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub bitrate: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: String,
    pub mime_type: String,
    pub quality: StreamQuality,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<StreamTrack>,
    pub albums: Vec<StreamAlbum>,
    pub artists: Vec<StreamArtist>,
    pub playlists: Vec<StreamPlaylist>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub username: Option<String>,
    pub subscription: Option<String>,
    pub expires_at: Option<String>,
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
}

#[async_trait::async_trait]
pub trait StreamingService: Send + Sync {
    fn name(&self) -> &str;
    fn enabled(&self) -> bool;

    async fn authenticate(&mut self, credentials: &serde_json::Value) -> Result<AuthStatus, String>;
    async fn auth_status(&self) -> AuthStatus;
    async fn logout(&mut self) -> Result<(), String>;

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, String>;
    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, String>;
    async fn get_track_url(&self, track_id: &str, quality: Option<&str>) -> Result<StreamUrl, String>;
    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, String>;
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, String>;
    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, String>;
    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, String>;
    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, String>;

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String>;
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String>;
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String>;

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, String> {
        Ok(vec![])
    }
    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, String> {
        Ok(vec![])
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        None
    }
    fn restore_tokens(&mut self, _tokens: &serde_json::Value) -> bool {
        false
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        Ok(false)
    }
}
