use serde::{Deserialize, Serialize};

use crate::TuneError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTrack {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(rename(serialize = "artist_name"), alias = "artist_name")]
    pub artist: String,
    #[serde(rename(serialize = "album_title"), alias = "album_title")]
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    pub cover_path: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub explicit: bool,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamAlbum {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(rename(serialize = "artist_name"), alias = "artist_name")]
    pub artist: String,
    pub artist_id: Option<String>,
    pub cover_path: Option<String>,
    pub year: Option<u32>,
    pub track_count: u32,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtist {
    pub id: String,
    pub name: String,
    pub image_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlaylist {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cover_path: Option<String>,
    pub track_count: u32,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamQuality {
    pub codec: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub bitrate: Option<u32>,
    #[serde(default = "default_channels")]
    pub channels: u16,
}

fn default_channels() -> u16 {
    2
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

/// A record label with (a page of) its albums. Qobuz `label/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelInfo {
    pub id: String,
    pub name: String,
    pub albums: Vec<StreamAlbum>,
}

/// An editorial playlist tag/category (Qobuz `playlist/getTags`): moods,
/// "Focus", genres… Used to browse curated playlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistTag {
    pub id: String,
    pub name: String,
}

/// Discovery context of an album/track: its genre and record label. Lets a
/// client jump from the now-playing track to the genre's expert playlists or
/// the label's catalogue, without bloating the shared `StreamAlbum` model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_name: Option<String>,
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
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    fn name(&self) -> &str;
    fn enabled(&self) -> bool;
    fn set_enabled(&mut self, enabled: bool);

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError>;
    async fn auth_status(&self) -> AuthStatus;
    async fn logout(&mut self) -> Result<(), TuneError>;

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError>;
    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError>;
    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError>;
    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError>;
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError>;
    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError>;
    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = artist_id;
        Ok(vec![])
    }
    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let _ = artist_id;
        Ok(vec![])
    }
    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError>;
    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError>;

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError>;
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError>;
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError>;

    async fn create_playlist(
        &self,
        _name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        Err("create_playlist not supported by this service".into())
    }
    async fn add_tracks_to_playlist(
        &self,
        _playlist_id: &str,
        _track_ids: &[String],
    ) -> Result<usize, TuneError> {
        Err("add_tracks_to_playlist not supported by this service".into())
    }
    async fn delete_playlist(&self, _playlist_id: &str) -> Result<(), TuneError> {
        Err("delete_playlist not supported by this service".into())
    }
    async fn remove_tracks_from_playlist(
        &self,
        _playlist_id: &str,
        _track_ids: &[String],
    ) -> Result<usize, TuneError> {
        Err("remove_tracks_from_playlist not supported by this service".into())
    }
    fn supports_write(&self) -> bool {
        false
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }

    async fn get_genres(&self, parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        let _ = parent_id;
        Ok(vec![])
    }
    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = (genre_id, limit);
        Ok(vec![])
    }
    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        Ok(vec![])
    }
    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = section_id;
        Ok(vec![])
    }
    /// Browse the record label of an album: resolves the album's label and
    /// returns it with its full catalogue. Album-based so the shared
    /// `StreamAlbum` model need not carry a label id.
    async fn get_album_label(&self, _album_id: &str) -> Result<LabelInfo, TuneError> {
        Err("labels not supported for this service".into())
    }
    /// Editorial playlist tags/categories (moods, "Focus", genres…).
    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        Ok(vec![])
    }
    /// Curated/editorial ("expert") playlists, optionally filtered by a tag id
    /// and/or a genre id.
    async fn get_featured_playlists(
        &self,
        _tag: Option<&str>,
        _genre: Option<&str>,
    ) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    /// Discovery context (genre + label) of an album, resolved from the album.
    async fn get_album_context(&self, _album_id: &str) -> Result<AlbumContext, TuneError> {
        Err("album context not supported for this service".into())
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let _ = (fav_type, item_id);
        Err("not supported".into())
    }
    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
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

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_track_serialization() {
        let track = StreamTrack {
            id: "123".into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: Some("Kind of Blue".into()),
            album_id: Some("456".into()),
            duration_ms: 562000,
            cover_path: Some("http://example.com/cover.jpg".into()),
            track_number: Some(1),
            disc_number: Some(1),
            explicit: false,
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 96000,
                bit_depth: 24,
                bitrate: None,
                channels: 2,
            }),
        };
        let json = serde_json::to_value(&track).unwrap();
        // id should serialize as "source_id" due to rename
        assert_eq!(json["source_id"], "123");
        assert_eq!(json["title"], "So What");
        // artist serializes as "artist_name" for web client compatibility
        assert_eq!(json["artist_name"], "Miles Davis");
        // album serializes as "album_title" for web client compatibility
        assert_eq!(json["album_title"], "Kind of Blue");
        assert_eq!(json["cover_path"], "http://example.com/cover.jpg");
        assert_eq!(json["duration_ms"], 562000);
    }

    #[test]
    fn stream_track_deserialization_with_source_id() {
        let json = serde_json::json!({
            "source_id": "abc",
            "title": "Test",
            "artist": "Test Artist",
            "duration_ms": 1000,
            "explicit": false,
        });
        let track: StreamTrack = serde_json::from_value(json).unwrap();
        assert_eq!(track.id, "abc");
        assert_eq!(track.title, "Test");
    }

    #[test]
    fn stream_album_serialization() {
        let album = StreamAlbum {
            id: "789".into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            artist_id: Some("42".into()),
            cover_path: Some("http://cover.jpg".into()),
            year: Some(1959),
            track_count: 5,
            quality: None,
            ..Default::default()
        };
        let json = serde_json::to_value(&album).unwrap();
        assert_eq!(json["source_id"], "789");
        assert_eq!(json["title"], "Kind of Blue");
        // artist serializes as "artist_name" for web client compatibility
        assert_eq!(json["artist_name"], "Miles Davis");
        assert_eq!(json["year"], 1959);
        assert_eq!(json["track_count"], 5);
    }

    #[test]
    fn stream_artist_serialization() {
        let artist = StreamArtist {
            id: "42".into(),
            name: "Miles Davis".into(),
            image_path: Some("http://img.jpg".into()),
        };
        let json = serde_json::to_value(&artist).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["name"], "Miles Davis");
        assert_eq!(json["image_path"], "http://img.jpg");
    }

    #[test]
    fn stream_playlist_serialization() {
        let playlist = StreamPlaylist {
            id: "pl-1".into(),
            name: "My Playlist".into(),
            description: Some("A great playlist".into()),
            cover_path: None,
            track_count: 10,
            owner: Some("testuser".into()),
        };
        let json = serde_json::to_value(&playlist).unwrap();
        assert_eq!(json["source_id"], "pl-1");
        assert_eq!(json["name"], "My Playlist");
        assert_eq!(json["track_count"], 10);
        assert!(json["cover_path"].is_null());
    }

    #[test]
    fn stream_quality_serialization() {
        let quality = StreamQuality {
            codec: "FLAC".into(),
            sample_rate: 192000,
            bit_depth: 24,
            bitrate: Some(9216),
            channels: 2,
        };
        let json = serde_json::to_value(&quality).unwrap();
        assert_eq!(json["codec"], "FLAC");
        assert_eq!(json["sample_rate"], 192000);
        assert_eq!(json["bit_depth"], 24);
        assert_eq!(json["bitrate"], 9216);
    }

    #[test]
    fn stream_url_serialization() {
        let url = StreamUrl {
            url: "https://stream.example.com/track.flac".into(),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 44100,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: Some(1700000000),
        };
        let json = serde_json::to_value(&url).unwrap();
        assert_eq!(json["url"], "https://stream.example.com/track.flac");
        assert_eq!(json["mime_type"], "audio/flac");
        assert_eq!(json["expires_at"], 1700000000);
    }

    #[test]
    fn search_results_serialization() {
        let results = SearchResults {
            tracks: vec![],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        };
        let json = serde_json::to_value(&results).unwrap();
        assert!(json["tracks"].as_array().unwrap().is_empty());
        assert!(json["albums"].as_array().unwrap().is_empty());
    }

    #[test]
    fn auth_status_default() {
        let status = AuthStatus::default();
        assert!(!status.authenticated);
        assert!(status.username.is_none());
        assert!(status.subscription.is_none());
        assert!(status.verification_url.is_none());
        assert!(status.user_code.is_none());
    }

    #[test]
    fn auth_status_serialization() {
        let status = AuthStatus {
            authenticated: true,
            username: Some("testuser".into()),
            subscription: Some("Premium".into()),
            expires_at: Some("3600s".into()),
            verification_url: None,
            user_code: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["authenticated"], true);
        assert_eq!(json["username"], "testuser");
        assert_eq!(json["subscription"], "Premium");
    }

    #[test]
    fn stream_genre_serialization() {
        let genre = StreamGenre {
            id: "jazz".into(),
            name: "Jazz".into(),
            has_children: true,
            image_url: Some("http://img.jpg".into()),
        };
        let json = serde_json::to_value(&genre).unwrap();
        assert_eq!(json["id"], "jazz");
        assert_eq!(json["has_children"], true);
    }

    #[test]
    fn featured_section_serialization() {
        let section = FeaturedSection {
            id: "new-releases".into(),
            name: "New Releases".into(),
        };
        let json = serde_json::to_value(&section).unwrap();
        assert_eq!(json["id"], "new-releases");
        assert_eq!(json["name"], "New Releases");
    }
}
