use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackCredit {
    pub name: String,
    pub role: String,
    pub instrument: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub album_artist_sort: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_subtitle: Option<String>,
    pub year: Option<u32>,
    pub original_year: Option<u32>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
    pub genre: Option<String>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub format: Option<String>,
    pub file_size: Option<u64>,
    pub bpm: Option<f64>,
    pub compilation: bool,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
    pub musicbrainz_album_artist_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub isrc: Option<String>,
    pub has_cover: bool,
    pub credits: Vec<TrackCredit>,
}

pub fn read_metadata(path: &Path) -> Option<TrackMetadata> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::{Accessor, ItemKey};

    let tagged = lofty::read_from_path(path).ok()?;
    let props = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;

    let get = |key: &ItemKey| tag.get_string(key).map(|s| s.to_string());

    let compilation_str = get(&ItemKey::FlagCompilation).unwrap_or_default();
    let compilation = matches!(compilation_str.as_str(), "1" | "true" | "True");

    let bpm = get(&ItemKey::Bpm).and_then(|s| s.parse::<f64>().ok());

    let original_year = get(&ItemKey::OriginalReleaseDate)
        .and_then(|s| s.get(..4)?.parse::<u32>().ok());

    let credits = parse_credits(tag);

    Some(TrackMetadata {
        title: tag.title().map(|s| s.to_string()),
        artist: tag.artist().map(|s| s.to_string()),
        album: tag.album().map(|s| s.to_string()),
        album_artist: get(&ItemKey::AlbumArtist),
        album_artist_sort: get(&ItemKey::AlbumArtistSortOrder),
        track_number: tag.track(),
        disc_number: tag.disk(),
        disc_subtitle: get(&ItemKey::SetSubtitle),
        year: tag.year(),
        original_year,
        release_date: get(&ItemKey::ReleaseDate),
        original_date: get(&ItemKey::OriginalReleaseDate),
        genre: tag.genre().map(|s| s.to_string()),
        duration_ms: Some(props.duration().as_millis() as u64),
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth().map(|b| b as u16),
        channels: props.channels().map(|c| c as u16),
        format: Some(format!("{:?}", tagged.file_type()).to_lowercase()),
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        bpm,
        compilation,
        label: get(&ItemKey::Label),
        catalog_number: get(&ItemKey::CatalogNumber),
        musicbrainz_recording_id: get(&ItemKey::MusicBrainzRecordingId),
        musicbrainz_release_id: get(&ItemKey::MusicBrainzReleaseId),
        musicbrainz_artist_id: get(&ItemKey::MusicBrainzArtistId),
        musicbrainz_album_artist_id: get(&ItemKey::MusicBrainzReleaseArtistId),
        musicbrainz_release_group_id: get(&ItemKey::MusicBrainzReleaseGroupId),
        isrc: get(&ItemKey::Isrc),
        has_cover: !tag.pictures().is_empty(),
        credits,
    })
}

pub struct MetadataUpdate {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
    pub composer: Option<String>,
    pub label: Option<String>,
}

pub fn write_metadata(path: &Path, update: &MetadataUpdate) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey, TagItem, ItemValue, TagExt};

    let mut tagged = lofty::read_from_path(path).map_err(|e| format!("read: {e}"))?;
    let tag = tagged.primary_tag_mut().ok_or("no primary tag")?;

    if let Some(ref v) = update.title { tag.set_title(v.clone()); }
    if let Some(ref v) = update.artist { tag.set_artist(v.clone()); }
    if let Some(ref v) = update.album { tag.set_album(v.clone()); }
    if let Some(ref v) = update.genre { tag.set_genre(v.clone()); }
    if let Some(v) = update.track_number { tag.set_track(v); }
    if let Some(v) = update.disc_number { tag.set_disk(v); }
    if let Some(v) = update.year { tag.set_year(v); }

    if let Some(ref v) = update.album_artist {
        tag.insert(TagItem::new(ItemKey::AlbumArtist, ItemValue::Text(v.clone())));
    }
    if let Some(ref v) = update.composer {
        tag.insert(TagItem::new(ItemKey::Composer, ItemValue::Text(v.clone())));
    }
    if let Some(ref v) = update.label {
        tag.insert(TagItem::new(ItemKey::Label, ItemValue::Text(v.clone())));
    }

    tag.save_to_path(path, WriteOptions::default()).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn parse_credits(tag: &lofty::tag::Tag) -> Vec<TrackCredit> {
    use lofty::tag::ItemKey;

    let mut credits = Vec::new();

    if let Some(composer) = tag.get_string(&ItemKey::Composer) {
        credits.push(TrackCredit {
            name: composer.to_string(),
            role: "composer".into(),
            instrument: None,
        });
    }

    if let Some(conductor) = tag.get_string(&ItemKey::Conductor) {
        credits.push(TrackCredit {
            name: conductor.to_string(),
            role: "conductor".into(),
            instrument: None,
        });
    }

    if let Some(lyricist) = tag.get_string(&ItemKey::Lyricist) {
        credits.push(TrackCredit {
            name: lyricist.to_string(),
            role: "lyricist".into(),
            instrument: None,
        });
    }

    for item in tag.items() {
        if item.key() == &ItemKey::Performer
            && let Some(val) = item.value().text() {
                let (name, instrument) = if let Some((n, i)) = val.split_once('(') {
                    (n.trim().to_string(), Some(i.trim_end_matches(')').trim().to_string()))
                } else {
                    (val.to_string(), None)
                };
                credits.push(TrackCredit {
                    name,
                    role: "performer".into(),
                    instrument,
                });
            }
    }

    credits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonexistent_file_returns_none() {
        assert!(read_metadata(Path::new("/tmp/nonexistent.flac")).is_none());
    }
}
