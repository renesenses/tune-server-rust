use super::traits::*;

pub struct YouTubeService {
    authenticated: bool,
    enabled_override: Option<bool>,
}

impl Default for YouTubeService {
    fn default() -> Self {
        Self::new()
    }
}

impl YouTubeService {
    pub fn new() -> Self {
        Self {
            authenticated: false,
            enabled_override: None,
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for YouTubeService {
    fn name(&self) -> &str {
        "youtube"
    }
    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(true)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, String> {
        Err("YouTube Music OAuth not yet implemented".into())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.authenticated,
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), String> {
        self.authenticated = false;
        Ok(())
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, String> {
        Err("not authenticated".into())
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, String> {
        Err("not authenticated".into())
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, String> {
        Err("not authenticated".into())
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, String> {
        Err("not authenticated".into())
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, String> {
        Err("not authenticated".into())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, String> {
        Err("not authenticated".into())
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, String> {
        Err("not authenticated".into())
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, String> {
        Err("not authenticated".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, String> {
        Err("not authenticated".into())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, String> {
        Err("not authenticated".into())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, String> {
        Err("not authenticated".into())
    }
}
