use serde::Serialize;

use tune_core::db::models::{Album, Artist, Track};
use tune_core::playback::NowPlaying;
use tune_core::streaming::traits::{StreamAlbum, StreamArtist, StreamTrack};

// ---------------------------------------------------------------------------
// TrackResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TrackResponse {
    pub id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub album_id: Option<i64>,
    pub cover_path: Option<String>,
    pub duration_ms: Option<i64>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
}

impl From<&Track> for TrackResponse {
    fn from(t: &Track) -> Self {
        Self {
            id: t.id,
            title: t.title.clone(),
            artist_name: t.artist_name.clone(),
            album_title: t.album_title.clone(),
            album_id: t.album_id,
            cover_path: t.cover_path.clone(),
            duration_ms: Some(t.duration_ms),
            source: Some(t.source.clone()),
            source_id: t.source_id.clone(),
            format: t.format.clone(),
            sample_rate: t.sample_rate.map(|v| v as i64),
            bit_depth: t.bit_depth.map(|v| v as i64),
            channels: Some(t.channels as i64),
            track_number: Some(t.track_number as i64),
            disc_number: Some(t.disc_number as i64),
        }
    }
}

impl From<Track> for TrackResponse {
    fn from(t: Track) -> Self {
        Self::from(&t)
    }
}

impl From<&StreamTrack> for TrackResponse {
    fn from(t: &StreamTrack) -> Self {
        Self {
            id: None,
            title: t.title.clone(),
            artist_name: Some(t.artist.clone()),
            album_title: t.album.clone(),
            album_id: None,
            cover_path: t.cover_path.clone(),
            duration_ms: Some(t.duration_ms as i64),
            source: None, // caller should set the service name
            source_id: Some(t.id.clone()),
            format: t.quality.as_ref().map(|q| q.codec.clone()),
            sample_rate: t.quality.as_ref().map(|q| q.sample_rate as i64),
            bit_depth: t.quality.as_ref().map(|q| q.bit_depth as i64),
            channels: t.quality.as_ref().map(|q| q.channels as i64),
            track_number: t.track_number.map(|v| v as i64),
            disc_number: t.disc_number.map(|v| v as i64),
        }
    }
}

impl From<StreamTrack> for TrackResponse {
    fn from(t: StreamTrack) -> Self {
        Self::from(&t)
    }
}

impl From<&NowPlaying> for TrackResponse {
    fn from(np: &NowPlaying) -> Self {
        Self {
            id: np.track_id,
            title: np.title.clone(),
            artist_name: np.artist_name.clone(),
            album_title: np.album_title.clone(),
            album_id: None,
            cover_path: np.cover_path.clone(),
            duration_ms: Some(np.duration_ms),
            source: Some(np.source.clone()),
            source_id: np.source_id.clone(),
            format: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            track_number: None,
            disc_number: None,
        }
    }
}

impl From<NowPlaying> for TrackResponse {
    fn from(np: NowPlaying) -> Self {
        Self::from(&np)
    }
}

// ---------------------------------------------------------------------------
// AlbumResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AlbumResponse {
    pub id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub artist_id: Option<i64>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub cover_path: Option<String>,
    pub source_id: Option<String>,
    pub track_count: Option<i64>,
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

impl From<&Album> for AlbumResponse {
    fn from(a: &Album) -> Self {
        Self {
            id: a.id,
            title: a.title.clone(),
            artist_name: a.artist_name.clone(),
            artist_id: a.artist_id,
            year: a.year.map(|v| v as i64),
            genre: a.genre.clone(),
            cover_path: a.cover_path.clone(),
            source_id: a.source_id.clone(),
            track_count: a.track_count.map(|v| v as i64),
            format: a.format.clone(),
            sample_rate: a.sample_rate.map(|v| v as i64),
            bit_depth: a.bit_depth.map(|v| v as i64),
        }
    }
}

impl From<Album> for AlbumResponse {
    fn from(a: Album) -> Self {
        Self::from(&a)
    }
}

impl From<&StreamAlbum> for AlbumResponse {
    fn from(a: &StreamAlbum) -> Self {
        Self {
            id: None,
            title: a.title.clone(),
            artist_name: Some(a.artist.clone()),
            artist_id: None,
            year: a.year.map(|v| v as i64),
            genre: None,
            cover_path: a.cover_path.clone(),
            source_id: Some(a.id.clone()),
            track_count: Some(a.track_count as i64),
            format: a.quality.as_ref().map(|q| q.codec.clone()),
            sample_rate: a.quality.as_ref().map(|q| q.sample_rate as i64),
            bit_depth: a.quality.as_ref().map(|q| q.bit_depth as i64),
        }
    }
}

impl From<StreamAlbum> for AlbumResponse {
    fn from(a: StreamAlbum) -> Self {
        Self::from(&a)
    }
}

// ---------------------------------------------------------------------------
// ArtistResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ArtistResponse {
    pub id: Option<i64>,
    pub name: String,
    pub image_path: Option<String>,
    pub source_id: Option<String>,
}

impl From<&Artist> for ArtistResponse {
    fn from(a: &Artist) -> Self {
        Self {
            id: a.id,
            name: a.name.clone(),
            image_path: a.image_path.clone(),
            source_id: None,
        }
    }
}

impl From<Artist> for ArtistResponse {
    fn from(a: Artist) -> Self {
        Self::from(&a)
    }
}

impl From<&StreamArtist> for ArtistResponse {
    fn from(a: &StreamArtist) -> Self {
        Self {
            id: None,
            name: a.name.clone(),
            image_path: a.image_path.clone(),
            source_id: Some(a.id.clone()),
        }
    }
}

impl From<StreamArtist> for ArtistResponse {
    fn from(a: StreamArtist) -> Self {
        Self::from(&a)
    }
}

// ---------------------------------------------------------------------------
// ZoneResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ZoneResponse {
    pub id: i64,
    pub name: Option<String>,
    pub output_type: Option<String>,
    pub output_device_id: Option<String>,
    pub volume: f64,
    pub state: String,
    pub current_track: Option<TrackResponse>,
    pub position_ms: i64,
    pub queue_length: i64,
    pub queue_position: i64,
    pub muted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sent: Option<bool>,
}

// ---------------------------------------------------------------------------
// SearchResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub tracks: Vec<TrackResponse>,
    pub albums: Vec<AlbumResponse>,
    pub artists: Vec<ArtistResponse>,
}
