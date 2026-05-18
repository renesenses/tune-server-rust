use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub format: Option<String>,
    pub file_size: Option<u64>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
    pub isrc: Option<String>,
    pub has_cover: bool,
    pub extra: HashMap<String, String>,
}

pub fn read_metadata(path: &Path) -> Option<TrackMetadata> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;

    let tagged = lofty::read_from_path(path).ok()?;
    let props = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;

    Some(TrackMetadata {
        title: tag.title().map(|s| s.to_string()),
        artist: tag.artist().map(|s| s.to_string()),
        album: tag.album().map(|s| s.to_string()),
        album_artist: tag.get_string(&lofty::tag::ItemKey::AlbumArtist).map(|s| s.to_string()),
        track_number: tag.track(),
        disc_number: tag.disk(),
        year: tag.year(),
        genre: tag.genre().map(|s| s.to_string()),
        duration_ms: Some(props.duration().as_millis() as u64),
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth().map(|b| b as u16),
        channels: props.channels().map(|c| c as u16),
        format: Some(format!("{:?}", tagged.file_type())),
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        musicbrainz_recording_id: tag.get_string(&lofty::tag::ItemKey::MusicBrainzRecordingId).map(|s| s.to_string()),
        musicbrainz_release_id: tag.get_string(&lofty::tag::ItemKey::MusicBrainzReleaseId).map(|s| s.to_string()),
        musicbrainz_artist_id: tag.get_string(&lofty::tag::ItemKey::MusicBrainzArtistId).map(|s| s.to_string()),
        isrc: tag.get_string(&lofty::tag::ItemKey::Isrc).map(|s| s.to_string()),
        has_cover: !tag.pictures().is_empty(),
        extra: HashMap::new(),
    })
}
