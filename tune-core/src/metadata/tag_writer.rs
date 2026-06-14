use std::collections::HashMap;
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::*;
use lofty::tag::ItemKey;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagUpdate {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub comment: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<i32>,
    pub label: Option<String>,
    pub lyrics: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagFormat {
    Id3,
    Vorbis,
    Mp4,
    Unknown,
}

pub fn detect_format(file_path: &str) -> TagFormat {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "mp3" => TagFormat::Id3,
        "flac" | "ogg" | "oga" | "opus" => TagFormat::Vorbis,
        "m4a" | "aac" | "alac" | "mp4" => TagFormat::Mp4,
        "aiff" | "aif" => TagFormat::Id3,
        _ => TagFormat::Unknown,
    }
}

pub async fn write_tags(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let format = detect_format(file_path);
    if format == TagFormat::Unknown {
        return Err("unsupported tag format".into());
    }
    if !Path::new(file_path).exists() {
        return Err("file not found".into());
    }
    let path = file_path.to_string();
    let update = update.clone();
    tokio::task::spawn_blocking(move || write_tags_lofty(&path, &update))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn write_tags_lofty(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let mut tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let tag_type = tagged.primary_tag_type();

    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let has_primary = tagged.primary_tag().is_some();
    let tag = if has_primary {
        tagged.primary_tag_mut().unwrap()
    } else {
        tagged.first_tag_mut().unwrap()
    };

    let mut count = 0usize;
    if let Some(ref v) = update.title {
        tag.set_title(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.artist_name {
        tag.set_artist(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.album_title {
        tag.set_album(v.clone());
        count += 1;
    }
    if let Some(v) = update.track_number {
        tag.set_track(v as u32);
        count += 1;
    }
    if let Some(v) = update.disc_number {
        tag.set_disk(v as u32);
        count += 1;
    }
    if let Some(ref v) = update.genre {
        tag.set_genre(v.clone());
        count += 1;
    }
    if let Some(v) = update.year {
        tag.set_date(lofty::tag::items::Timestamp {
            year: v as u16,
            ..Default::default()
        });
        count += 1;
    }
    if let Some(ref v) = update.comment {
        tag.set_comment(v.clone());
        count += 1;
    }

    if count == 0 {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .map_err(|e| format!("open: {e}"))?;
    tagged
        .save_to(&mut file, WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    info!(file = file_path, fields = count, "tags_written_lofty");
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

pub async fn read_tags(file_path: &str) -> Result<HashMap<String, String>, String> {
    let path = file_path.to_string();
    tokio::task::spawn_blocking(move || read_tags_lofty(&path))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn read_tags_lofty(file_path: &str) -> Result<HashMap<String, String>, String> {
    let tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let mut tags = HashMap::new();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return Ok(tags),
    };
    if let Some(v) = tag.title() {
        tags.insert("title".into(), v.to_string());
    }
    if let Some(v) = tag.artist() {
        tags.insert("artist".into(), v.to_string());
    }
    if let Some(v) = tag.album() {
        tags.insert("album".into(), v.to_string());
    }
    if let Some(v) = tag.genre() {
        tags.insert("genre".into(), v.to_string());
    }
    if let Some(v) = tag.date() {
        tags.insert("date".into(), v.to_string());
    }
    if let Some(v) = tag.track() {
        tags.insert("tracknumber".into(), v.to_string());
    }
    if let Some(v) = tag.disk() {
        tags.insert("discnumber".into(), v.to_string());
    }
    if let Some(v) = tag.comment() {
        tags.insert("comment".into(), v.to_string());
    }
    debug!(file = file_path, count = tags.len(), "tags_read_lofty");
    Ok(tags)
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub file_path: String,
    pub fields_written: usize,
}

// --- Extended metadata writing (HashMap-based) ---

/// Map a Tune metadata field name to the corresponding lofty `ItemKey`.
/// These keys match the ones used in `read_extended_metadata`.
fn tune_key_to_lofty(key: &str) -> Option<ItemKey> {
    match key {
        // Credits / personnel
        "composer" => Some(ItemKey::Composer),
        "conductor" => Some(ItemKey::Conductor),
        "lyricist" => Some(ItemKey::Lyricist),
        "performer" => Some(ItemKey::Performer),
        "remixer" => Some(ItemKey::Remixer),
        "label" => Some(ItemKey::Label),
        "producer" => Some(ItemKey::Producer),

        // Descriptive
        "bpm" => Some(ItemKey::Bpm),
        "mood" => Some(ItemKey::Mood),
        "comment" => Some(ItemKey::Comment),
        "lyrics" => Some(ItemKey::Lyrics),
        "grouping" => Some(ItemKey::ContentGroup),
        "compilation" => Some(ItemKey::FlagCompilation),

        // Identifiers
        "isrc" => Some(ItemKey::Isrc),
        "barcode" => Some(ItemKey::Barcode),
        "catalog_number" => Some(ItemKey::CatalogNumber),
        "media_type" => Some(ItemKey::OriginalMediaType),

        // Dates
        "release_date" => Some(ItemKey::ReleaseDate),
        "original_date" => Some(ItemKey::OriginalReleaseDate),

        // Technical
        "copyright" => Some(ItemKey::CopyrightMessage),
        "language" => Some(ItemKey::Language),
        "encoder" => Some(ItemKey::EncodedBy),

        // Sort order
        "sort_artist" => Some(ItemKey::TrackArtistSortOrder),
        "sort_album" => Some(ItemKey::AlbumTitleSortOrder),
        "sort_album_artist" => Some(ItemKey::AlbumArtistSortOrder),

        // Core fields (album_artist written via ItemKey)
        "album_artist" => Some(ItemKey::AlbumArtist),

        // MusicBrainz IDs
        "mb_track_id" => Some(ItemKey::MusicBrainzRecordingId),
        "mb_release_id" => Some(ItemKey::MusicBrainzReleaseId),
        "mb_artist_id" => Some(ItemKey::MusicBrainzArtistId),
        "mb_release_artist_id" => Some(ItemKey::MusicBrainzReleaseArtistId),
        "mb_release_group_id" => Some(ItemKey::MusicBrainzReleaseGroupId),
        "mb_work_id" => Some(ItemKey::MusicBrainzWorkId),

        // ReplayGain (read-only typically, but allow writing)
        "rg_track_gain" => Some(ItemKey::ReplayGainTrackGain),
        "rg_track_peak" => Some(ItemKey::ReplayGainTrackPeak),
        "rg_album_gain" => Some(ItemKey::ReplayGainAlbumGain),
        "rg_album_peak" => Some(ItemKey::ReplayGainAlbumPeak),

        _ => None,
    }
}

/// Returns true if the file extension is not supported for tag writing.
fn is_unsupported_format(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    // DFF has no standard tag support
    matches!(ext.as_str(), "dff")
}

/// Write extended metadata fields to an audio file's tags.
///
/// For each key in `fields`:
/// - If the value is empty, the corresponding tag item is removed.
/// - If the value is non-empty, the tag item is inserted/replaced.
///
/// Skips unsupported formats (DFF) and missing files gracefully.
pub async fn write_metadata_to_file(
    file_path: &str,
    fields: &HashMap<String, String>,
) -> Result<WriteResult, String> {
    if is_unsupported_format(file_path) {
        return Err("unsupported format for tag writing".into());
    }
    if !Path::new(file_path).exists() {
        return Err("file not found".into());
    }
    let path = file_path.to_string();
    let fields = fields.clone();
    tokio::task::spawn_blocking(move || write_metadata_to_file_sync(&path, &fields))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn write_metadata_to_file_sync(
    file_path: &str,
    fields: &HashMap<String, String>,
) -> Result<WriteResult, String> {
    let mut tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let tag_type = tagged.primary_tag_type();

    // Ensure we have a tag to write to
    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let has_primary = tagged.primary_tag().is_some();
    let tag = if has_primary {
        tagged.primary_tag_mut().unwrap()
    } else {
        tagged.first_tag_mut().unwrap()
    };

    let mut count = 0usize;
    for (key, value) in fields {
        let Some(item_key) = tune_key_to_lofty(key) else {
            debug!(key = key.as_str(), "tag_writer_unknown_key_skipped");
            continue;
        };

        if value.is_empty() {
            // Remove the tag item
            tag.remove_key(item_key);
        } else {
            // Insert/replace the tag item
            tag.insert_text(item_key, value.clone());
        }
        count += 1;
    }

    if count == 0 {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .map_err(|e| format!("open: {e}"))?;
    tagged
        .save_to(&mut file, WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    info!(file = file_path, fields = count, "extended_tags_written");
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mp3() {
        assert_eq!(detect_format("/music/song.mp3"), TagFormat::Id3);
    }
    #[test]
    fn detect_flac() {
        assert_eq!(detect_format("/music/song.flac"), TagFormat::Vorbis);
    }
    #[test]
    fn detect_m4a() {
        assert_eq!(detect_format("/music/song.m4a"), TagFormat::Mp4);
    }
    #[test]
    fn detect_unknown() {
        assert_eq!(detect_format("/music/song.xyz"), TagFormat::Unknown);
    }

    #[test]
    fn tune_key_mapping_covers_all_extended_fields() {
        // Verify all keys from read_extended_metadata have a mapping
        let keys = [
            "composer",
            "conductor",
            "lyricist",
            "performer",
            "remixer",
            "label",
            "producer",
            "bpm",
            "mood",
            "comment",
            "lyrics",
            "grouping",
            "compilation",
            "isrc",
            "barcode",
            "catalog_number",
            "media_type",
            "release_date",
            "original_date",
            "copyright",
            "language",
            "encoder",
            "sort_artist",
            "sort_album",
            "sort_album_artist",
            "album_artist",
            "mb_track_id",
            "mb_release_id",
            "mb_artist_id",
            "mb_release_artist_id",
            "mb_release_group_id",
            "mb_work_id",
            "rg_track_gain",
            "rg_track_peak",
            "rg_album_gain",
            "rg_album_peak",
        ];
        for key in keys {
            assert!(
                tune_key_to_lofty(key).is_some(),
                "missing mapping for key: {key}"
            );
        }
    }

    #[test]
    fn tune_key_mapping_returns_none_for_unknown() {
        assert!(tune_key_to_lofty("unknown_field").is_none());
        assert!(tune_key_to_lofty("").is_none());
    }

    #[test]
    fn unsupported_format_dff() {
        assert!(is_unsupported_format("/music/track.dff"));
        assert!(is_unsupported_format("/music/track.DFF"));
    }

    #[test]
    fn supported_formats_not_blocked() {
        assert!(!is_unsupported_format("/music/track.flac"));
        assert!(!is_unsupported_format("/music/track.mp3"));
        assert!(!is_unsupported_format("/music/track.m4a"));
        assert!(!is_unsupported_format("/music/track.dsf"));
    }
}
