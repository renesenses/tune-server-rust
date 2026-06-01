pub mod artist_enrichment;
pub mod lyrics;
pub mod auto_fix;
pub mod batch;
pub mod credit_enricher;
pub mod enrichment;
pub mod fingerprint;
pub mod lastfm;
pub mod matcher;
pub mod musicbrainz_release;
pub mod suggestions;
pub mod tag_writer;

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
    pub total_tracks: Option<u32>,
    pub total_discs: Option<u32>,
    pub disc_subtitle: Option<String>,
    pub year: Option<u32>,
    pub original_year: Option<u32>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
    /// Primary genre (first after splitting multi-genre tags)
    pub genre: Option<String>,
    /// All genres parsed from the tag (split by `;`, `/`, `\\`)
    pub genres: Vec<String>,
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

/// Split a multi-genre tag string into individual genres.
///
/// Handles common separators: `;`, `/`, `\\`, and `\0` (null byte, used by
/// some ID3v2 implementations for multi-value frames).
///
/// Examples:
///   "Jazz; Fusion; Progressive" -> ["Jazz", "Fusion", "Progressive"]
///   "Jazz/Fusion/Progressive"   -> ["Jazz", "Fusion", "Progressive"]
///   "Rock"                      -> ["Rock"]
///   ""                          -> []
pub fn split_genre_tag(raw: &str) -> Vec<String> {
    // Split by semicolon, forward-slash, backslash, or null byte
    raw.split(&[';', '/', '\\', '\0'][..])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Normalize a lofty `FileType` debug string into a user-friendly format name.
///
/// lofty's `FileType` Debug representation doesn't always match what users expect:
///   - `Mpeg` -> `mp3`
///   - `Dsf`  -> `dsd` (DSD over PCM, stored in .dsf container)
///   - `Dff`  -> `dsd` (DSD Interchange File Format)
///   - Other values pass through unchanged (already lowercase).
pub fn normalize_format(raw: &str) -> String {
    match raw {
        "mpeg" => "mp3".to_string(),
        "dsf" | "dff" => "dsd".to_string(),
        "mp4" | "m4a" => "aac".to_string(),
        other => other.to_string(),
    }
}

/// Fallback metadata extraction for DSF/DFF files when lofty fails.
///
/// Reads the DSF file header to extract sample rate, channel count, and
/// duration.  For DFF files (or if DSF header parsing fails), returns a
/// minimal `TrackMetadata` with at least the title, format, and file size.
fn dsf_dff_fallback(path: &Path) -> Option<TrackMetadata> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "dsf" && ext != "dff" {
        return None;
    }

    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let file_size = std::fs::metadata(path).ok().map(|m| m.len());

    // Derive album from parent dir, artist from grandparent (Artist/Album/track.dsf)
    let album = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string());
    let artist = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string());

    let (sample_rate, channels, duration_ms) = if ext == "dsf" {
        parse_dsf_header(path).unwrap_or((None, None, None))
    } else {
        (None, None, None)
    };

    Some(TrackMetadata {
        title: Some(title),
        album,
        artist: artist.clone(),
        album_artist: artist,
        format: Some("dsd".to_string()),
        file_size,
        sample_rate,
        channels,
        duration_ms: duration_ms.or(Some(0)),
        ..Default::default()
    })
}

/// Parse a DSF file header to extract sample rate, channel count, and duration.
#[allow(clippy::type_complexity)]
fn parse_dsf_header(path: &Path) -> Result<(Option<u32>, Option<u16>, Option<u64>), ()> {
    use std::io::Read;

    let mut f = std::fs::File::open(path).map_err(|_| ())?;
    let mut header = [0u8; 92]; // DSD chunk (28) + fmt chunk header (64 is plenty)
    f.read_exact(&mut header).map_err(|_| ())?;

    // Verify "DSD " magic
    if &header[0..4] != b"DSD " {
        return Err(());
    }

    // DSD chunk size (bytes 4-11, little-endian u64) — should be 28
    // Total file size at bytes 12-19
    // Metadata offset at bytes 20-27

    // fmt chunk should start at offset 28
    if &header[28..32] != b"fmt " {
        return Err(());
    }

    // fmt chunk: offset 28 is "fmt ", 32-39 is chunk size (u64 LE)
    // 40-43: format version
    // 44-47: format ID
    // 48-51: channel type
    // 52-55: channel count (u32 LE)
    // 56-59: sample rate (u32 LE)
    // 60-61: bits per sample (u16? actually u32 at 60-63)
    // 64-71: sample count per channel (u64 LE)

    let channels = u32::from_le_bytes([header[52], header[53], header[54], header[55]]);
    let sample_rate = u32::from_le_bytes([header[56], header[57], header[58], header[59]]);
    let bits_per_sample = u32::from_le_bytes([header[60], header[61], header[62], header[63]]);
    let sample_count = u64::from_le_bytes([
        header[64], header[65], header[66], header[67], header[68], header[69], header[70],
        header[71],
    ]);

    let duration_ms = if sample_rate > 0 {
        // DSD sample rate is 1-bit rate (e.g. 2_822_400 for DSD64).
        // Duration = sample_count / sample_rate * 1000
        Some(sample_count * 1000 / sample_rate as u64)
    } else {
        None
    };

    let _ = bits_per_sample; // typically 1 for DSD

    Ok((Some(sample_rate), Some(channels as u16), duration_ms))
}

pub fn try_read_metadata(path: &Path) -> Result<TrackMetadata, String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = match Probe::open(path).and_then(|p| {
        p.options(ParseOptions::new().parsing_mode(ParsingMode::Relaxed))
            .guess_file_type()?
            .read()
    }) {
        Ok(t) => t,
        Err(e) => return dsf_dff_fallback(path).ok_or_else(|| format!("{e}")),
    };
    let props = tagged.properties();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return dsf_dff_fallback(path).ok_or_else(|| "no tags found".to_string()),
    };

    let get = |key: &ItemKey| tag.get_string(key).map(|s| s.to_string());

    let compilation_str = get(&ItemKey::FlagCompilation).unwrap_or_default();
    let compilation = matches!(compilation_str.as_str(), "1" | "true" | "True");

    let bpm = get(&ItemKey::Bpm).and_then(|s| s.parse::<f64>().ok());

    let original_year =
        get(&ItemKey::OriginalReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok());

    let total_tracks = tag
        .track_total()
        .or_else(|| get(&ItemKey::TrackTotal).and_then(|s| s.parse::<u32>().ok()));
    let total_discs = tag
        .disk_total()
        .or_else(|| get(&ItemKey::DiscTotal).and_then(|s| s.parse::<u32>().ok()));

    let credits = parse_credits(tag);

    let raw_genre = tag.genre().map(|s| s.to_string());
    let genres = raw_genre
        .as_deref()
        .map(split_genre_tag)
        .unwrap_or_default();
    let genre = genres.first().cloned().or(raw_genre);

    Ok(TrackMetadata {
        title: tag.title().map(|s| s.to_string()),
        artist: tag.artist().map(|s| s.to_string()),
        album: tag.album().map(|s| s.to_string()),
        album_artist: get(&ItemKey::AlbumArtist),
        album_artist_sort: get(&ItemKey::AlbumArtistSortOrder),
        track_number: tag.track(),
        disc_number: tag.disk(),
        total_tracks,
        total_discs,
        disc_subtitle: get(&ItemKey::SetSubtitle),
        year: tag.year(),
        original_year,
        release_date: get(&ItemKey::ReleaseDate),
        original_date: get(&ItemKey::OriginalReleaseDate),
        genre,
        genres,
        duration_ms: Some(props.duration().as_millis() as u64),
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth().map(|b| b as u16),
        channels: props.channels().map(|c| c as u16),
        format: Some(normalize_format(
            &format!("{:?}", tagged.file_type()).to_lowercase(),
        )),
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

pub fn read_metadata(path: &Path) -> Option<TrackMetadata> {
    try_read_metadata(path).ok()
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
    use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagItem};

    let mut tagged = lofty::read_from_path(path).map_err(|e| format!("read: {e}"))?;
    let tag = tagged.primary_tag_mut().ok_or("no primary tag")?;

    if let Some(ref v) = update.title {
        tag.set_title(v.clone());
    }
    if let Some(ref v) = update.artist {
        tag.set_artist(v.clone());
    }
    if let Some(ref v) = update.album {
        tag.set_album(v.clone());
    }
    if let Some(ref v) = update.genre {
        tag.set_genre(v.clone());
    }
    if let Some(v) = update.track_number {
        tag.set_track(v);
    }
    if let Some(v) = update.disc_number {
        tag.set_disk(v);
    }
    if let Some(v) = update.year {
        tag.set_year(v);
    }

    if let Some(ref v) = update.album_artist {
        tag.insert(TagItem::new(
            ItemKey::AlbumArtist,
            ItemValue::Text(v.clone()),
        ));
    }
    if let Some(ref v) = update.composer {
        tag.insert(TagItem::new(ItemKey::Composer, ItemValue::Text(v.clone())));
    }
    if let Some(ref v) = update.label {
        tag.insert(TagItem::new(ItemKey::Label, ItemValue::Text(v.clone())));
    }

    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| format!("write: {e}"))?;
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
            && let Some(val) = item.value().text()
        {
            let (name, instrument) = if let Some((n, i)) = val.split_once('(') {
                (
                    n.trim().to_string(),
                    Some(i.trim_end_matches(')').trim().to_string()),
                )
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

    #[test]
    fn split_genre_semicolon() {
        let genres = split_genre_tag("Jazz; Fusion; Progressive");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn split_genre_slash() {
        let genres = split_genre_tag("Jazz/Fusion/Progressive");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn split_genre_backslash() {
        let genres = split_genre_tag("Rock\\Metal\\Punk");
        assert_eq!(genres, vec!["Rock", "Metal", "Punk"]);
    }

    #[test]
    fn split_genre_null_byte() {
        let genres = split_genre_tag("Jazz\0Blues\0Soul");
        assert_eq!(genres, vec!["Jazz", "Blues", "Soul"]);
    }

    #[test]
    fn split_genre_single() {
        let genres = split_genre_tag("Jazz");
        assert_eq!(genres, vec!["Jazz"]);
    }

    #[test]
    fn split_genre_empty() {
        let genres = split_genre_tag("");
        assert!(genres.is_empty());
    }

    #[test]
    fn split_genre_mixed_separators() {
        let genres = split_genre_tag("Jazz; Rock/Blues");
        assert_eq!(genres, vec!["Jazz", "Rock", "Blues"]);
    }

    #[test]
    fn split_genre_trims_whitespace() {
        let genres = split_genre_tag("  Jazz ;  Fusion  ; Progressive  ");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn split_genre_consecutive_separators() {
        let genres = split_genre_tag("Jazz;;Rock");
        assert_eq!(genres, vec!["Jazz", "Rock"]);
    }

    #[test]
    fn split_genre_only_separators() {
        let genres = split_genre_tag(";;;");
        assert!(genres.is_empty());
    }

    #[test]
    fn split_genre_unicode() {
        let genres = split_genre_tag("Musique classique; Musique experimentale");
        assert_eq!(genres, vec!["Musique classique", "Musique experimentale"]);
    }

    #[test]
    fn split_genre_single_char() {
        let genres = split_genre_tag("A");
        assert_eq!(genres, vec!["A"]);
    }

    #[test]
    fn track_metadata_default() {
        let md = TrackMetadata::default();
        assert!(md.title.is_none());
        assert!(md.artist.is_none());
        assert!(md.genres.is_empty());
        assert!(!md.compilation);
        assert!(!md.has_cover);
        assert!(md.credits.is_empty());
    }

    #[test]
    fn track_metadata_serialization() {
        let md = TrackMetadata {
            title: Some("Test".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            genre: Some("Jazz".into()),
            genres: vec!["Jazz".into(), "Fusion".into()],
            duration_ms: Some(300_000),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            format: Some("flac".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        assert_eq!(json["title"], "Test");
        assert_eq!(json["genres"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn track_credit_default() {
        let credit = TrackCredit::default();
        assert_eq!(credit.name, "");
        assert_eq!(credit.role, "");
        assert!(credit.instrument.is_none());
    }

    #[test]
    fn metadata_update_fields() {
        let update = MetadataUpdate {
            title: Some("New Title".into()),
            artist: Some("New Artist".into()),
            album: None,
            album_artist: None,
            genre: Some("Rock".into()),
            track_number: Some(1),
            disc_number: Some(1),
            year: Some(2024),
            composer: Some("Composer".into()),
            label: None,
        };
        assert_eq!(update.title.as_deref(), Some("New Title"));
        assert_eq!(update.year, Some(2024));
    }

    #[test]
    fn normalize_format_mpeg_to_mp3() {
        assert_eq!(normalize_format("mpeg"), "mp3");
    }

    #[test]
    fn normalize_format_dsf_to_dsd() {
        assert_eq!(normalize_format("dsf"), "dsd");
    }

    #[test]
    fn normalize_format_dff_to_dsd() {
        assert_eq!(normalize_format("dff"), "dsd");
    }

    #[test]
    fn normalize_format_flac_unchanged() {
        assert_eq!(normalize_format("flac"), "flac");
    }

    #[test]
    fn normalize_format_wav_unchanged() {
        assert_eq!(normalize_format("wav"), "wav");
    }

    #[test]
    fn normalize_format_aiff_unchanged() {
        assert_eq!(normalize_format("aiff"), "aiff");
    }

    #[test]
    fn dsf_dff_fallback_returns_none_for_non_dsd() {
        assert!(dsf_dff_fallback(Path::new("/tmp/test.flac")).is_none());
        assert!(dsf_dff_fallback(Path::new("/tmp/test.mp3")).is_none());
    }

    #[test]
    fn dsf_dff_fallback_returns_dsd_format() {
        // Even if the file does not exist, the fallback should return
        // a minimal metadata with format "dsd" for .dsf / .dff extensions.
        let meta = dsf_dff_fallback(Path::new("/tmp/nonexistent_track.dsf"));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.format.as_deref(), Some("dsd"));
        assert_eq!(meta.title.as_deref(), Some("nonexistent_track"));
        assert_eq!(meta.duration_ms, Some(0));

        let meta2 = dsf_dff_fallback(Path::new("/tmp/test_track.dff"));
        assert!(meta2.is_some());
        let meta2 = meta2.unwrap();
        assert_eq!(meta2.format.as_deref(), Some("dsd"));
        assert_eq!(meta2.title.as_deref(), Some("test_track"));
    }

    #[test]
    fn dsf_fallback_with_valid_header() {
        use std::io::Write;
        // Build a minimal DSF file header to test header parsing.
        // DSD chunk (28 bytes) + fmt chunk (52 bytes)
        let tmp = std::env::temp_dir().join("tune_test_dsf_fallback.dsf");
        let mut buf = vec![0u8; 92];
        // DSD chunk
        buf[0..4].copy_from_slice(b"DSD ");
        // DSD chunk size = 28 (LE u64)
        buf[4..12].copy_from_slice(&28u64.to_le_bytes());
        // total file size (doesn't matter for test)
        buf[12..20].copy_from_slice(&92u64.to_le_bytes());
        // metadata offset = 0
        buf[20..28].copy_from_slice(&0u64.to_le_bytes());
        // fmt chunk
        buf[28..32].copy_from_slice(b"fmt ");
        // fmt chunk size = 52 (LE u64)
        buf[32..40].copy_from_slice(&52u64.to_le_bytes());
        // format version = 1
        buf[40..44].copy_from_slice(&1u32.to_le_bytes());
        // format ID = 0
        buf[44..48].copy_from_slice(&0u32.to_le_bytes());
        // channel type = 2 (stereo)
        buf[48..52].copy_from_slice(&2u32.to_le_bytes());
        // channel count = 2
        buf[52..56].copy_from_slice(&2u32.to_le_bytes());
        // sample rate = 2_822_400 (DSD64)
        buf[56..60].copy_from_slice(&2_822_400u32.to_le_bytes());
        // bits per sample = 1
        buf[60..64].copy_from_slice(&1u32.to_le_bytes());
        // sample count per channel = 2_822_400 * 180 (= 3 minutes at DSD64)
        let samples: u64 = 2_822_400 * 180;
        buf[64..72].copy_from_slice(&samples.to_le_bytes());

        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(&buf)
            .unwrap();

        let meta = dsf_dff_fallback(&tmp);
        std::fs::remove_file(&tmp).ok();

        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.format.as_deref(), Some("dsd"));
        assert_eq!(meta.sample_rate, Some(2_822_400));
        assert_eq!(meta.channels, Some(2));
        // Duration should be approximately 180_000 ms (3 minutes)
        let dur = meta.duration_ms.unwrap();
        assert!(
            (179_000..=181_000).contains(&dur),
            "unexpected duration: {dur}ms"
        );
    }

    #[test]
    fn try_read_metadata_dsf_fallback() {
        // A nonexistent .dsf file should trigger the fallback and return Ok
        // (not an error), since we recognize the extension.
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.dsf"));
        assert!(result.is_ok());
        let meta = result.unwrap();
        assert_eq!(meta.format.as_deref(), Some("dsd"));
    }

    #[test]
    fn try_read_metadata_non_dsd_still_errors() {
        // A nonexistent .flac file should still return Err
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.flac"));
        assert!(result.is_err());
    }

    // ── Parsing robustness tests ────────────────────────────────────

    #[test]
    fn normalize_format_mp4_variants() {
        assert_eq!(normalize_format("mp4"), "aac");
        assert_eq!(normalize_format("m4a"), "aac");
    }

    #[test]
    fn normalize_format_unknown_passthrough() {
        assert_eq!(normalize_format("ogg"), "ogg");
        assert_eq!(normalize_format("opus"), "opus");
        assert_eq!(normalize_format("wv"), "wv");
        assert_eq!(normalize_format("ape"), "ape");
    }

    #[test]
    fn split_genre_parenthesized_id3v1_numeric() {
        // Some taggers write "(17)" for Rock — our splitter doesn't decode
        // numeric ID3v1 codes, but it should not crash.
        let genres = split_genre_tag("(17)Rock");
        assert!(!genres.is_empty());
    }

    #[test]
    fn split_genre_very_long() {
        let long = (0..50)
            .map(|i| format!("Genre{i}"))
            .collect::<Vec<_>>()
            .join(";");
        let genres = split_genre_tag(&long);
        assert_eq!(genres.len(), 50);
    }

    #[test]
    fn track_metadata_all_optional_fields_none() {
        let md = TrackMetadata::default();
        let json = serde_json::to_value(&md).unwrap();
        assert!(json["title"].is_null());
        assert!(json["artist"].is_null());
        assert!(json["album"].is_null());
        assert!(json["album_artist"].is_null());
        assert!(json["track_number"].is_null());
        assert!(json["disc_number"].is_null());
        assert!(json["year"].is_null());
        assert!(json["sample_rate"].is_null());
        assert!(json["bit_depth"].is_null());
        assert!(json["duration_ms"].is_null());
        assert_eq!(json["compilation"], false);
        assert_eq!(json["has_cover"], false);
    }

    #[test]
    fn track_metadata_json_types_stable() {
        let md = TrackMetadata {
            title: Some("Track".into()),
            track_number: Some(3),
            disc_number: Some(1),
            total_tracks: Some(12),
            total_discs: Some(2),
            year: Some(2024),
            duration_ms: Some(245_000),
            sample_rate: Some(96000),
            bit_depth: Some(24),
            channels: Some(2),
            bpm: Some(120.5),
            compilation: true,
            has_cover: true,
            genres: vec!["Jazz".into(), "Fusion".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();

        assert!(json["track_number"].is_number());
        assert!(json["disc_number"].is_number());
        assert!(json["total_tracks"].is_number());
        assert!(json["total_discs"].is_number());
        assert!(json["year"].is_number());
        assert!(json["duration_ms"].is_number());
        assert!(json["sample_rate"].is_number());
        assert!(json["bit_depth"].is_number());
        assert!(json["channels"].is_number());
        assert!(json["bpm"].is_number());
        assert_eq!(json["compilation"], true);
        assert_eq!(json["has_cover"], true);
        assert!(json["genres"].is_array());
        assert_eq!(json["genres"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn track_metadata_credits_serialization() {
        let md = TrackMetadata {
            credits: vec![
                TrackCredit {
                    name: "John Doe".into(),
                    role: "composer".into(),
                    instrument: None,
                },
                TrackCredit {
                    name: "Jane Doe".into(),
                    role: "performer".into(),
                    instrument: Some("piano".into()),
                },
            ],
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        let credits = json["credits"].as_array().unwrap();
        assert_eq!(credits.len(), 2);
        assert_eq!(credits[0]["name"], "John Doe");
        assert_eq!(credits[0]["role"], "composer");
        assert!(credits[0]["instrument"].is_null());
        assert_eq!(credits[1]["instrument"], "piano");
    }

    #[test]
    fn track_metadata_musicbrainz_ids_serialization() {
        let md = TrackMetadata {
            musicbrainz_recording_id: Some("rec-uuid".into()),
            musicbrainz_release_id: Some("rel-uuid".into()),
            musicbrainz_artist_id: Some("art-uuid".into()),
            musicbrainz_album_artist_id: Some("aa-uuid".into()),
            musicbrainz_release_group_id: Some("rg-uuid".into()),
            isrc: Some("USRC12345678".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        assert_eq!(json["musicbrainz_recording_id"], "rec-uuid");
        assert_eq!(json["musicbrainz_release_id"], "rel-uuid");
        assert_eq!(json["musicbrainz_artist_id"], "art-uuid");
        assert_eq!(json["musicbrainz_album_artist_id"], "aa-uuid");
        assert_eq!(json["musicbrainz_release_group_id"], "rg-uuid");
        assert_eq!(json["isrc"], "USRC12345678");
    }

    #[test]
    fn dsf_fallback_derives_album_and_artist_from_path() {
        let meta = dsf_dff_fallback(Path::new("/music/Miles Davis/Kind of Blue/01-So What.dsf"));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("01-So What"));
        assert_eq!(meta.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(meta.artist.as_deref(), Some("Miles Davis"));
    }

    #[test]
    fn normalize_format_case_sensitivity() {
        // normalize_format expects lowercase input (from format!("{:?}").to_lowercase())
        assert_eq!(normalize_format("mpeg"), "mp3");
        // uppercase should pass through unchanged (it's pre-lowercased)
        assert_eq!(normalize_format("MPEG"), "MPEG");
    }
}
