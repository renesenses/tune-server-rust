use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTrack {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(alias = "artist_name")]
    pub artist: String,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    #[serde(rename(serialize = "cover_path"), alias = "cover_path")]
    pub cover_url: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub explicit: bool,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamAlbum {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(alias = "artist_name")]
    pub artist: String,
    pub artist_id: Option<String>,
    #[serde(rename(serialize = "cover_path"), alias = "cover_path")]
    pub cover_url: Option<String>,
    pub year: Option<u32>,
    pub track_count: u32,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtist {
    pub id: String,
    pub name: String,
    #[serde(rename(serialize = "image_path"), alias = "image_path")]
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlaylist {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename(serialize = "cover_path"), alias = "cover_path")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamGenre {
    pub id: String,
    pub name: String,
    pub has_children: bool,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturedSection {
    pub id: String,
    pub name: String,
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
    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let _ = artist_id;
        Ok(vec![])
    }
    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, String> {
        let _ = artist_id;
        Ok(vec![])
    }
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

    async fn get_genres(&self) -> Result<Vec<StreamGenre>, String> {
        Ok(vec![])
    }
    async fn get_genre_albums(&self, genre_id: &str, limit: usize) -> Result<Vec<StreamAlbum>, String> {
        let _ = (genre_id, limit);
        Ok(vec![])
    }
    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, String> {
        Ok(vec![])
    }
    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let _ = section_id;
        Ok(vec![])
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, String> {
        Ok(vec![])
    }
    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let _ = (fav_type, item_id);
        Err("not supported".into())
    }
    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), String> {
        let _ = (fav_type, item_id);
        Err("not supported".into())
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        None
    }
    fn restore_tokens(&mut self, _tokens: &serde_json::Value) -> bool {
        false
    }

    async fn post_restore(&mut self) {}

    async fn refresh_if_needed(&mut self) -> Result<bool, String> {
        Ok(false)
    }
}
