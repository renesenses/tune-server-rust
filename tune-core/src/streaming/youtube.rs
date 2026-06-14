use super::traits::*;
use crate::TuneError;

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
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
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
    ) -> Result<AuthStatus, TuneError> {
        Err("YouTube Music OAuth not yet implemented".into())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.authenticated,
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.authenticated = false;
        Ok(())
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Err("not authenticated".into())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Err("not authenticated".into())
    }
}
