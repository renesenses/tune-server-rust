pub mod artist_enrichment;
pub mod auto_fix;
pub mod batch;
pub mod bio_batch;
pub mod credit_enricher;
pub mod enrichment;
pub mod fingerprint;
pub mod lastfm;
pub mod lyrics;
pub mod matcher;
pub mod musicbrainz_release;
pub mod suggestions;
pub mod tag_writer;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    pub comment: Option<String>,
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
/// Normalize a genre string to Title Case, handling special tokens.
///
/// - Splits on whitespace, capitalizes the first letter of each word and
///   lowercases the rest.
/// - Preserves well-known uppercase tokens: "R&B", "DJ", "UK", "US", "MC",
///   "TV", "AC", "DC", "EDM", "RnB", "II", "III", "IV".
/// - Handles slash-separated sub-genres (e.g. "Folk/Rock") by normalizing
///   each part independently.
///
/// Examples:
///   "classique"       -> "Classique"
///   "ROCK"            -> "Rock"
///   "r&b"             -> "R&B"
///   "hip hop"         -> "Hip Hop"
///   "dj mix"          -> "DJ Mix"
///   "folk/rock"       -> "Folk/Rock"
pub fn normalize_genre(genre: &str) -> String {
    // Uppercase tokens that must be preserved verbatim (checked case-insensitively)
    const UPPERCASE_TOKENS: &[&str] = &[
        "R&B", "DJ", "UK", "US", "MC", "TV", "AC", "DC", "EDM", "II", "III", "IV",
    ];

    fn title_case_word(word: &str) -> String {
        // Check if the whole word matches an uppercase token
        for &token in UPPERCASE_TOKENS {
            if word.eq_ignore_ascii_case(token) {
                return token.to_string();
            }
        }
        // Title-case: first char uppercase, rest lowercase
        let mut chars = word.chars();
        match chars.next() {
            None => String::new(),
            Some(first) => {
                let mut s = first.to_uppercase().to_string();
                for c in chars {
                    s.extend(c.to_lowercase());
                }
                s
            }
        }
    }

    // Handle slash-separated compound genres like "Folk/Rock"
    genre
        .split('/')
        .map(|part| {
            part.split_whitespace()
                .map(title_case_word)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn split_genre_tag(raw: &str) -> Vec<String> {
    // Split by semicolon, forward-slash, backslash, or null byte
    raw.split(&[';', '/', '\\', '\0'][..])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(normalize_genre)
        .collect()
}

/// Normalize a lofty `FileType` debug string into a user-friendly format name.
///
/// lofty's `FileType` Debug representation doesn't always match what users expect:
///   - `Mpeg` -> `mp3`
///   - `Dsf`  -> `dsd` (DSD over PCM, stored in .dsf container)
///   - `Dff`  -> `dsd` (DSD Interchange File Format)
///   - `Mp4`  -> `alac` when bit_depth is present (ALAC is lossless, has bit depth)
///             -> `aac` otherwise (AAC is lossy, no bit depth reported by lofty)
///   - Other values pass through unchanged (already lowercase).
pub fn normalize_format(raw: &str, bit_depth: Option<u8>) -> String {
    match raw {
        "mpeg" => "mp3".to_string(),
        "dsf" | "dff" => "dsd".to_string(),
        "mp4" | "m4a" => {
            // ALAC (Apple Lossless) files in M4A containers report a bit depth
            // (typically 16 or 24), while AAC (lossy) does not.
            if bit_depth.is_some() {
                "alac".to_string()
            } else {
                "aac".to_string()
            }
        }
        // lofty may report "alac" directly for some M4A files
        "alac" => "alac".to_string(),
        other => other.to_string(),
    }
}

/// Detect ALAC vs AAC for M4A files by probing with symphonia.
/// Returns "alac" if the codec is ALAC, "aac" otherwise.
pub fn probe_m4a_codec(path: &std::path::Path) -> Option<String> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let format_reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    let track = format_reader.default_track(symphonia::core::formats::TrackType::Audio)?;
    let codec_name = format!("{:?}", track.codec_params);
    if codec_name.contains("Alac") || codec_name.contains("alac") || codec_name.contains("ALAC") {
        Some("alac".to_string())
    } else {
        Some("aac".to_string())
    }
}

// ── DSF / DFF support ──────────────────────────────────────────────────

/// Parsed DSF header information.
struct DsfHeaderInfo {
    sample_rate: Option<u32>,
    channels: Option<u16>,
    duration_ms: Option<u64>,
    /// Byte offset of the ID3v2 metadata chunk (0 means no metadata).
    metadata_offset: Option<u64>,
}

/// Parse a DSF file header to extract sample rate, channel count, duration,
/// and the metadata (ID3v2) offset.
fn parse_dsf_header_full(path: &Path) -> Result<DsfHeaderInfo, ()> {
    use std::io::Read;

    let mut f = std::fs::File::open(path).map_err(|_| ())?;
    let mut header = [0u8; 92]; // DSD chunk (28) + fmt chunk header (64 is plenty)
    f.read_exact(&mut header).map_err(|_| ())?;

    // Verify "DSD " magic
    if &header[0..4] != b"DSD " {
        return Err(());
    }

    // DSD chunk: bytes 4-11 = chunk size (u64 LE, should be 28)
    //            bytes 12-19 = total file size
    //            bytes 20-27 = metadata offset (0 = none)
    let metadata_offset = u64::from_le_bytes([
        header[20], header[21], header[22], header[23], header[24], header[25], header[26],
        header[27],
    ]);

    // fmt chunk should start at offset 28
    if &header[28..32] != b"fmt " {
        return Err(());
    }

    // fmt chunk layout (all little-endian):
    //   28-31: "fmt " magic
    //   32-39: chunk size (u64)
    //   40-43: format version (u32)
    //   44-47: format ID (u32)
    //   48-51: channel type (u32)
    //   52-55: channel count (u32)
    //   56-59: sample rate (u32)
    //   60-63: bits per sample (u32)
    //   64-71: sample count per channel (u64)
    let channels = u32::from_le_bytes([header[52], header[53], header[54], header[55]]);
    let sample_rate = u32::from_le_bytes([header[56], header[57], header[58], header[59]]);
    let bits_per_sample = u32::from_le_bytes([header[60], header[61], header[62], header[63]]);
    let sample_count = u64::from_le_bytes([
        header[64], header[65], header[66], header[67], header[68], header[69], header[70],
        header[71],
    ]);

    let duration_ms = if sample_rate > 0 {
        // DSD sample rate is 1-bit rate (e.g. 2_822_400 for DSD64).
        Some(sample_count * 1000 / sample_rate as u64)
    } else {
        None
    };

    let _ = bits_per_sample; // typically 1 for DSD

    Ok(DsfHeaderInfo {
        sample_rate: Some(sample_rate),
        channels: Some(channels as u16),
        duration_ms,
        metadata_offset: if metadata_offset > 0 {
            Some(metadata_offset)
        } else {
            None
        },
    })
}

/// A parsed ID3v2 text frame (frame ID -> text value).
#[derive(Debug, Default)]
struct Id3v2Tags {
    /// Standard text frames: frame_id (e.g. "TIT2") -> value
    text_frames: Vec<(String, String)>,
    /// TXXX user-defined text frames: description -> value
    txxx_frames: Vec<(String, String)>,
    /// Whether an APIC (picture) frame was found
    has_picture: bool,
}

impl Id3v2Tags {
    /// Get the first text frame matching the given ID.
    fn get(&self, frame_id: &str) -> Option<&str> {
        self.text_frames
            .iter()
            .find(|(id, _)| id == frame_id)
            .map(|(_, v)| v.as_str())
    }

    /// Get a TXXX frame by description (case-insensitive).
    fn get_txxx(&self, description: &str) -> Option<&str> {
        self.txxx_frames
            .iter()
            .find(|(desc, _)| desc.eq_ignore_ascii_case(description))
            .map(|(_, v)| v.as_str())
    }

    fn title(&self) -> Option<&str> {
        self.get("TIT2")
    }
    fn artist(&self) -> Option<&str> {
        self.get("TPE1")
    }
    fn album(&self) -> Option<&str> {
        self.get("TALB")
    }
    fn album_artist(&self) -> Option<&str> {
        self.get("TPE2")
    }
    fn genre(&self) -> Option<&str> {
        self.get("TCON")
    }

    /// Parse track number from TRCK frame ("7" or "7/11").
    fn track_number(&self) -> Option<u32> {
        let raw = self.get("TRCK")?;
        raw.split('/').next()?.trim().parse().ok()
    }

    /// Parse total tracks from TRCK frame ("7/11").
    fn total_tracks(&self) -> Option<u32> {
        let raw = self.get("TRCK")?;
        raw.split('/').nth(1)?.trim().parse().ok()
    }

    /// Parse disc number from TPOS frame ("1" or "1/2").
    fn disc_number(&self) -> Option<u32> {
        let raw = self.get("TPOS")?;
        raw.split('/').next()?.trim().parse().ok()
    }

    /// Parse total discs from TPOS frame ("1/2").
    fn total_discs(&self) -> Option<u32> {
        let raw = self.get("TPOS")?;
        raw.split('/').nth(1)?.trim().parse().ok()
    }

    /// Parse year from TDRC, TYER, TDRL, or TDOR frame (in priority order).
    fn year(&self) -> Option<u32> {
        self.get("TDRC")
            .or_else(|| self.get("TYER"))
            .or_else(|| self.get("TDRL"))
            .or_else(|| self.get("TDOR"))
            .and_then(|s| s.get(..4)?.parse().ok())
    }

    fn disc_subtitle(&self) -> Option<&str> {
        self.get("TSST")
    }

    fn release_date(&self) -> Option<&str> {
        self.get("TDRL")
    }

    fn label(&self) -> Option<&str> {
        self.get("TPUB")
    }

    fn composer(&self) -> Option<&str> {
        self.get("TCOM")
    }

    fn album_artist_sort(&self) -> Option<&str> {
        self.get("TSO2").or_else(|| self.get("TSOA"))
    }

    fn original_date(&self) -> Option<&str> {
        self.get("TDOR")
    }

    fn original_year(&self) -> Option<u32> {
        self.original_date().and_then(|s| s.get(..4)?.parse().ok())
    }

    fn isrc(&self) -> Option<&str> {
        self.get("TSRC")
    }
}

/// Decode an ID3v2 syncsafe integer (7 bits per byte).
fn syncsafe_to_u32(bytes: &[u8]) -> u32 {
    debug_assert!(bytes.len() == 4);
    ((bytes[0] as u32) << 21)
        | ((bytes[1] as u32) << 14)
        | ((bytes[2] as u32) << 7)
        | (bytes[3] as u32)
}

/// Read and parse an ID3v2 tag from a byte slice starting at "ID3".
///
/// Supports ID3v2.3 and ID3v2.4 text frames (TIT2, TPE1, TALB, etc.)
/// and TXXX user-defined text frames. Skips binary frames (APIC, etc.)
/// but notes their presence.
fn parse_id3v2_tag(data: &[u8]) -> Option<Id3v2Tags> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let major_version = data[3]; // 3 = ID3v2.3, 4 = ID3v2.4
    let _minor_version = data[4];
    let flags = data[5];
    let tag_size = syncsafe_to_u32(&data[6..10]) as usize;

    // We only handle ID3v2.3 and ID3v2.4
    if major_version < 3 || major_version > 4 {
        return None;
    }

    // Check for extended header
    let mut pos = 10;
    if flags & 0x40 != 0 {
        // Extended header present, skip it
        if pos + 4 > data.len() {
            return None;
        }
        let ext_size = if major_version == 4 {
            syncsafe_to_u32(&data[pos..pos + 4]) as usize
        } else {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        };
        pos += ext_size.max(4);
    }

    let tag_end = (10 + tag_size).min(data.len());
    let mut tags = Id3v2Tags::default();

    while pos + 10 <= tag_end {
        // Frame header: 4 bytes ID, 4 bytes size, 2 bytes flags
        let frame_id = match std::str::from_utf8(&data[pos..pos + 4]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };

        // Stop on padding (null bytes)
        if frame_id.starts_with('\0') {
            break;
        }

        let frame_size = if major_version == 4 {
            syncsafe_to_u32(&data[pos + 4..pos + 8]) as usize
        } else {
            // ID3v2.3 uses regular big-endian u32 for frame size
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        };

        let _frame_flags = u16::from_be_bytes([data[pos + 8], data[pos + 9]]);
        pos += 10; // skip frame header

        if frame_size == 0 || pos + frame_size > tag_end {
            break;
        }

        let frame_data = &data[pos..pos + frame_size];
        pos += frame_size;

        // Check for picture frames
        if frame_id == "APIC" {
            tags.has_picture = true;
            continue;
        }

        // Only process text frames (start with 'T') and TXXX
        if !frame_id.starts_with('T') {
            continue;
        }

        // Text frame: first byte is encoding, rest is the string
        if frame_data.is_empty() {
            continue;
        }

        let encoding = frame_data[0];
        let text_data = &frame_data[1..];

        let text = decode_id3v2_string(encoding, text_data);
        let text = text.trim_end_matches('\0').trim().to_string();

        if text.is_empty() {
            continue;
        }

        if frame_id == "TXXX" {
            // TXXX: encoding byte + null-terminated description + value
            // The `text` we decoded contains "description\0value"
            if let Some(null_pos) = text.find('\0') {
                let desc = text[..null_pos].trim().to_string();
                let val = text[null_pos + 1..].trim().to_string();
                if !desc.is_empty() && !val.is_empty() {
                    tags.txxx_frames.push((desc, val));
                }
            }
        } else {
            tags.text_frames.push((frame_id, text));
        }
    }

    Some(tags)
}

/// Decode an ID3v2 text string given its encoding byte.
///
/// Encodings:
///   0 = ISO-8859-1 (Latin-1)
///   1 = UTF-16 with BOM
///   2 = UTF-16BE without BOM
///   3 = UTF-8
fn decode_id3v2_string(encoding: u8, data: &[u8]) -> String {
    match encoding {
        0 => {
            // ISO-8859-1: each byte maps directly to a Unicode code point
            data.iter().map(|&b| b as char).collect()
        }
        1 => {
            // UTF-16 with BOM
            if data.len() < 2 {
                return String::new();
            }
            let is_le = data[0] == 0xFF && data[1] == 0xFE;
            let payload = &data[2..];
            decode_utf16(payload, is_le)
        }
        2 => {
            // UTF-16BE without BOM
            decode_utf16(data, false)
        }
        3 => {
            // UTF-8
            String::from_utf8_lossy(data).to_string()
        }
        _ => String::from_utf8_lossy(data).to_string(),
    }
}

/// Decode a UTF-16 byte slice to a String.
fn decode_utf16(data: &[u8], little_endian: bool) -> String {
    let pairs = data.chunks_exact(2);
    let code_units: Vec<u16> = pairs
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&code_units)
}

/// Read and parse the ID3v2 metadata chunk from a DSF file.
///
/// DSF files store an ID3v2 tag at the byte offset specified in the DSD
/// chunk header (bytes 20-27).
fn read_dsf_id3v2_tags(path: &Path, metadata_offset: Option<u64>) -> Option<Id3v2Tags> {
    use std::io::{Read, Seek, SeekFrom};

    let offset = metadata_offset?;

    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();

    // Sanity check: offset must be within the file, with room for at least
    // the 10-byte ID3v2 header.
    if offset + 10 > file_len {
        return None;
    }

    f.seek(SeekFrom::Start(offset)).ok()?;

    // Read the ID3v2 header to get the tag size
    let mut header = [0u8; 10];
    f.read_exact(&mut header).ok()?;

    if &header[0..3] != b"ID3" {
        return None;
    }

    let tag_size = syncsafe_to_u32(&header[6..10]) as usize;
    let total_tag_bytes = 10 + tag_size;

    // Cap read at 1 MB to avoid OOM on corrupt files
    if total_tag_bytes > 1_048_576 {
        return None;
    }

    // Read the full tag into memory
    let mut tag_data = vec![0u8; total_tag_bytes];
    tag_data[..10].copy_from_slice(&header);
    f.read_exact(&mut tag_data[10..]).ok()?;

    parse_id3v2_tag(&tag_data)
}

/// Fallback metadata extraction for DSF/DFF files when lofty fails.
///
/// DSF files contain an ID3v2 tag at an offset specified in the DSD chunk
/// header (bytes 20-27).  This function reads that offset, seeks to the
/// ID3v2 data, and parses the embedded tags.  Audio properties (sample rate,
/// channels, duration) come from the fmt chunk header.
///
/// For DFF files (or if DSF header / ID3v2 parsing fails), we fall back to
/// deriving title/album/artist from the file path.
fn dsf_dff_fallback(path: &Path) -> Option<TrackMetadata> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "dsf" && ext != "dff" {
        return None;
    }

    let file_size = std::fs::metadata(path).ok().map(|m| m.len());

    let (sample_rate, channels, duration_ms, metadata_offset) = if ext == "dsf" {
        match parse_dsf_header_full(path) {
            Ok(info) => (
                info.sample_rate,
                info.channels,
                info.duration_ms,
                info.metadata_offset,
            ),
            Err(_) => (None, None, None, None),
        }
    } else {
        (None, None, None, None)
    };

    // Try to read ID3v2 tags from the DSF metadata chunk
    let id3_tags = if ext == "dsf" {
        read_dsf_id3v2_tags(path, metadata_offset)
    } else {
        None
    };

    let (
        title,
        artist,
        album,
        album_artist,
        album_artist_sort,
        track_number,
        disc_number,
        total_tracks,
        total_discs,
        disc_subtitle,
        year,
        original_year,
        original_date,
        release_date,
        genre,
        genres,
        has_cover,
        label,
        isrc,
        compilation,
        credits,
    ) = if let Some(ref tags) = id3_tags {
        let raw_genre = tags.genre().map(|s| s.to_string());
        let genres = raw_genre
            .as_deref()
            .map(split_genre_tag)
            .unwrap_or_default();
        let genre = genres.first().cloned().or(raw_genre);

        let compilation_str = tags.get("TCMP").unwrap_or("");
        let compilation = matches!(compilation_str, "1" | "true" | "True");

        let mut credits = Vec::new();
        if let Some(composer) = tags.composer() {
            credits.push(TrackCredit {
                name: composer.to_string(),
                role: "composer".into(),
                instrument: None,
            });
        }
        if let Some(conductor) = tags.get("TPE3") {
            credits.push(TrackCredit {
                name: conductor.to_string(),
                role: "conductor".into(),
                instrument: None,
            });
        }

        (
            tags.title().map(|s| s.to_string()),
            tags.artist().map(|s| s.to_string()),
            tags.album().map(|s| s.to_string()),
            tags.album_artist().map(|s| s.to_string()),
            tags.album_artist_sort().map(|s| s.to_string()),
            tags.track_number(),
            tags.disc_number(),
            tags.total_tracks(),
            tags.total_discs(),
            tags.disc_subtitle().map(|s| s.to_string()),
            tags.year(),
            tags.original_year(),
            tags.original_date().map(|s| s.to_string()),
            tags.release_date().map(|s| s.to_string()),
            genre,
            genres,
            tags.has_picture,
            tags.label().map(|s| s.to_string()),
            tags.isrc().map(|s| s.to_string()),
            compilation,
            credits,
        )
    } else {
        (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            false,
            None,
            None,
            false,
            Vec::new(),
        )
    };

    // Fall back to filename/directory for fields the ID3v2 tag didn't provide
    let title = title.or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()));
    let album = album.or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
    });
    let artist_fallback = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string());
    let artist = artist.or(artist_fallback.clone());
    let album_artist = album_artist.or(artist_fallback);

    // Extract MusicBrainz IDs from TXXX frames
    let (
        mb_recording_id,
        mb_release_id,
        mb_artist_id,
        mb_album_artist_id,
        mb_release_group_id,
        catalog_number,
    ) = if let Some(ref tags) = id3_tags {
        (
            tags.get_txxx("MusicBrainz Recording Id")
                .map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Album Id").map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Artist Id")
                .map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Album Artist Id")
                .map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Release Group Id")
                .map(|s| s.to_string()),
            tags.get_txxx("CATALOGNUMBER")
                .or_else(|| tags.get_txxx("CatalogNumber"))
                .map(|s| s.to_string()),
        )
    } else {
        (None, None, None, None, None, None)
    };

    Some(TrackMetadata {
        title,
        album,
        artist,
        album_artist,
        album_artist_sort,
        track_number,
        disc_number,
        total_tracks,
        total_discs,
        disc_subtitle,
        year,
        original_year,
        release_date,
        original_date,
        genre,
        genres,
        format: Some("dsd".to_string()),
        file_size,
        sample_rate,
        channels,
        duration_ms: duration_ms.or(Some(0)),
        bit_depth: Some(1), // DSD is always 1-bit
        bpm: None,
        compilation,
        label,
        catalog_number,
        musicbrainz_recording_id: mb_recording_id,
        musicbrainz_release_id: mb_release_id,
        musicbrainz_artist_id: mb_artist_id,
        musicbrainz_album_artist_id: mb_album_artist_id,
        musicbrainz_release_group_id: mb_release_group_id,
        isrc,
        has_cover,
        credits,
        comment: None,
    })
}

fn m4a_fallback(path: &Path) -> Option<TrackMetadata> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "m4a" && ext != "mp4" && ext != "alac" {
        return None;
    }
    let file_name = path.file_stem()?.to_str()?;
    let parent = path.parent()?;
    let album = parent.file_name()?.to_str().map(|s| s.to_string());
    let artist = parent
        .parent()?
        .file_name()?
        .to_str()
        .map(|s| s.to_string());

    let (track_number, title) =
        if let Some(rest) = file_name.strip_prefix(|c: char| c.is_ascii_digit()) {
            let num_str: String = std::iter::once(file_name.chars().next().unwrap())
                .chain(rest.chars().take_while(|c| c.is_ascii_digit()))
                .collect();
            let after = file_name[num_str.len()..].trim_start_matches([' ', '-', '.', '_']);
            (num_str.parse::<u32>().ok(), Some(after.to_string()))
        } else {
            (None, Some(file_name.to_string()))
        };

    let file_size = std::fs::metadata(path).ok().map(|m| m.len());

    tracing::debug!(path = %path.display(), title = ?title, artist = ?artist, album = ?album, "m4a_fallback_metadata");

    Some(TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number: None,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format: Some("alac".to_string()),
        file_size,
        sample_rate: None,
        channels: Some(2),
        duration_ms: None,
        bit_depth: None,
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        credits: vec![],
        comment: None,
    })
}

/// Check if a file has a known audio extension (used to decide whether to
/// attempt a filesystem-based metadata fallback when lofty fails).
fn is_known_audio_ext(path: &Path) -> bool {
    const AUDIO_EXTS: &[&str] = &[
        "flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif", "wv", "wma", "dsf", "dff",
        "dst", "alac", "ape",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| AUDIO_EXTS.contains(&ext.to_lowercase().as_str()))
}

/// Extract basic metadata from the directory structure when lofty successfully
/// parsed the audio properties but the file has no tags.
///
/// Directory convention: `.../Artist/Album/01 - Title.wav`
fn tagless_fallback(path: &Path, props: &lofty::properties::FileProperties) -> TrackMetadata {
    let (track_number, title) = extract_title_from_filename(path);
    let parent = path.parent();
    let album = parent
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let artist = parent
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();
    let format = {
        let mut fmt = normalize_format(&ext, props.bit_depth());
        if fmt == "aac" && (ext == "m4a" || ext == "mp4") && props.bit_depth().is_none() {
            if let Some(probed) = probe_m4a_codec(path) {
                fmt = probed;
            }
        }
        Some(fmt)
    };

    tracing::debug!(
        path = %path.display(),
        title = ?title,
        artist = ?artist,
        album = ?album,
        "tagless_fallback_metadata"
    );

    TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number: None,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format,
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        sample_rate: props.sample_rate(),
        channels: props.channels().map(|c| c as u16),
        duration_ms: Some(props.duration().as_millis() as u64),
        bit_depth: props.bit_depth().map(|b| b as u16),
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        credits: vec![],
        comment: None,
    }
}

/// Fallback when lofty cannot parse the file at all (no audio properties).
/// Extracts everything from the filesystem.
fn tagless_fallback_no_props(path: &Path) -> TrackMetadata {
    let (track_number, title) = extract_title_from_filename(path);
    let parent = path.parent();
    let album = parent
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let artist = parent
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();

    tracing::debug!(
        path = %path.display(),
        title = ?title,
        artist = ?artist,
        album = ?album,
        "tagless_fallback_no_props_metadata"
    );

    TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number: None,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format: Some(ext),
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        sample_rate: None,
        channels: Some(2),
        duration_ms: None,
        bit_depth: None,
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        credits: vec![],
        comment: None,
    }
}

/// Parse track number and title from a filename.
///
/// Handles patterns like:
///   "01 - Title.wav" -> (Some(1), Some("Title"))
///   "01. Title.wav"  -> (Some(1), Some("Title"))
///   "01_Title.wav"   -> (Some(1), Some("Title"))
///   "Title.wav"      -> (None, Some("Title"))
fn extract_title_from_filename(path: &Path) -> (Option<u32>, Option<String>) {
    let file_name = match path.file_stem().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return (None, None),
    };
    if let Some(first_char) = file_name.chars().next()
        && first_char.is_ascii_digit()
    {
        let num_str: String = file_name
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let after = file_name[num_str.len()..].trim_start_matches([' ', '-', '.', '_']);
        let title = if after.is_empty() {
            Some(file_name.to_string())
        } else {
            Some(after.to_string())
        };
        (num_str.parse::<u32>().ok(), title)
    } else {
        (None, Some(file_name.to_string()))
    }
}

fn mp3_duration_sanity_check(path: &Path, lofty_ms: u64) -> u64 {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_size == 0 || lofty_ms == 0 {
        return lofty_ms;
    }
    // Estimate duration from file size assuming ~320kbps max bitrate.
    // If lofty reports more than 2x this estimate, it's likely wrong.
    let max_bitrate_bps = 320_000u64;
    let max_plausible_ms = (file_size * 8 * 1000) / max_bitrate_bps;
    if lofty_ms > max_plausible_ms * 2 {
        tracing::warn!(
            path = %path.display(),
            lofty_ms,
            max_plausible_ms,
            file_size,
            "mp3_duration_implausible_clamping"
        );
        max_plausible_ms
    } else {
        lofty_ms
    }
}

fn raw_vorbis_field(path: &Path, field_name: &str) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !matches!(ext.as_str(), "flac" | "ogg" | "opus") {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let needle = format!("{}=", field_name);
    let needle_upper = format!("{}=", field_name.to_uppercase());
    let content = String::from_utf8_lossy(&data);
    for line_bytes in data.windows(needle.len()) {
        let chunk = std::str::from_utf8(line_bytes).unwrap_or("");
        if chunk.eq_ignore_ascii_case(&needle) {
            let start = (line_bytes.as_ptr() as usize) - (data.as_ptr() as usize) + needle.len();
            if start < data.len() {
                let rest = &data[start..];
                let end = rest
                    .iter()
                    .position(|&b| b == 0 || b < 0x20)
                    .unwrap_or(rest.len().min(512));
                let value = std::str::from_utf8(&rest[..end]).ok()?;
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    let _ = (content, needle_upper);
    None
}

pub fn try_read_metadata(path: &Path) -> Result<TrackMetadata, String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = match Probe::open(path).and_then(|p| {
        p.options(
            ParseOptions::new()
                .parsing_mode(ParsingMode::Relaxed)
                .max_junk_bytes(1024 * 1024),
        )
        .guess_file_type()?
        .read()
    }) {
        Ok(t) => t,
        Err(e) => {
            // Try DSF/DFF fallback first
            if let Some(meta) = dsf_dff_fallback(path) {
                return Ok(meta);
            }
            // For M4A/ALAC files that lofty can't parse (large atoms),
            // fall back to directory/filename-based metadata extraction
            if let Some(meta) = m4a_fallback(path) {
                return Ok(meta);
            }
            // For any other audio file (WAV, AIFF, etc.) that lofty cannot
            // parse, extract basic metadata from the filesystem so the file
            // still appears in the library rather than being silently skipped.
            // Only apply the fallback if the file actually exists (a missing
            // file should still return Err).
            if is_known_audio_ext(path) && path.exists() {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "lofty_parse_failed_using_filesystem_fallback"
                );
                return Ok(tagless_fallback_no_props(path));
            }
            return Err(format!("{e}"));
        }
    };
    let props = tagged.properties();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => {
            if let Some(meta) = dsf_dff_fallback(path) {
                return Ok(meta);
            }
            // For audio files that lofty can parse (valid audio properties)
            // but have no tags (e.g. WAV without RIFF INFO or ID3v2),
            // extract metadata from the file/directory structure so they
            // still appear in the library instead of being silently skipped.
            return Ok(tagless_fallback(path, props));
        }
    };

    let get = |key: ItemKey| tag.get_string(key).map(|s| s.to_string());

    let compilation_str = get(ItemKey::FlagCompilation).unwrap_or_default();
    let compilation = matches!(compilation_str.as_str(), "1" | "true" | "True");

    let bpm = get(ItemKey::Bpm).and_then(|s| s.parse::<f64>().ok());

    let original_year =
        get(ItemKey::OriginalReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok());

    let total_tracks = tag
        .track_total()
        .or_else(|| get(ItemKey::TrackTotal).and_then(|s| s.parse::<u32>().ok()));
    let total_discs = tag
        .disk_total()
        .or_else(|| get(ItemKey::DiscTotal).and_then(|s| s.parse::<u32>().ok()));

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
        album_artist: get(ItemKey::AlbumArtist).or_else(|| raw_vorbis_field(path, "album_artist")),
        album_artist_sort: get(ItemKey::AlbumArtistSortOrder),
        track_number: tag.track(),
        disc_number: tag.disk(),
        total_tracks,
        total_discs,
        disc_subtitle: get(ItemKey::SetSubtitle),
        year: tag
            .date()
            .map(|d| d.year as u32)
            .or_else(|| {
                // Fallback: try TDRL (ReleaseDate), then TDOR (OriginalReleaseDate)
                get(ItemKey::ReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok())
            })
            .or_else(|| {
                get(ItemKey::OriginalReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok())
            }),
        original_year,
        release_date: get(ItemKey::ReleaseDate),
        original_date: get(ItemKey::OriginalReleaseDate),
        genre,
        genres,
        duration_ms: {
            let lofty_dur = props.duration().as_millis() as u64;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if ext == "mp3" {
                Some(mp3_duration_sanity_check(path, lofty_dur))
            } else {
                Some(lofty_dur)
            }
        },
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth().map(|b| b as u16),
        channels: props.channels().map(|c| c as u16),
        format: Some(normalize_format(
            &format!("{:?}", tagged.file_type()).to_lowercase(),
            props.bit_depth(),
        )),
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        bpm,
        compilation,
        label: get(ItemKey::Label),
        catalog_number: get(ItemKey::CatalogNumber),
        musicbrainz_recording_id: get(ItemKey::MusicBrainzRecordingId),
        musicbrainz_release_id: get(ItemKey::MusicBrainzReleaseId),
        musicbrainz_artist_id: get(ItemKey::MusicBrainzArtistId),
        musicbrainz_album_artist_id: get(ItemKey::MusicBrainzReleaseArtistId),
        musicbrainz_release_group_id: get(ItemKey::MusicBrainzReleaseGroupId),
        isrc: get(ItemKey::Isrc),
        has_cover: !tag.pictures().is_empty(),
        credits,
        comment: tag.comment().map(|s| s.to_string()),
    })
}

pub fn read_metadata(path: &Path) -> Option<TrackMetadata> {
    try_read_metadata(path).ok()
}

/// Read extended metadata tags beyond the core fields already stored in the tracks table.
/// Returns a HashMap of key-value pairs suitable for the track_metadata table.
/// This extracts tags like composer, conductor, lyricist, performer, remixer,
/// ReplayGain values, MusicBrainz IDs, and other extended fields.
pub fn read_extended_metadata(path: &Path) -> HashMap<String, String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let mut meta = HashMap::new();

    let tagged = match Probe::open(path).and_then(|p| {
        p.options(
            ParseOptions::new()
                .parsing_mode(ParsingMode::Relaxed)
                .max_junk_bytes(1024 * 1024),
        )
        .guess_file_type()?
        .read()
    }) {
        Ok(t) => t,
        Err(_) => return meta,
    };

    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return meta,
    };

    let get = |key: ItemKey| tag.get_string(key).map(|s| s.to_string());

    // Sort-order fields
    if let Some(v) = get(ItemKey::TrackArtistSortOrder) {
        meta.insert("sort_artist".into(), v);
    }
    if let Some(v) = get(ItemKey::AlbumTitleSortOrder) {
        meta.insert("sort_album".into(), v);
    }
    if let Some(v) = get(ItemKey::AlbumArtistSortOrder) {
        meta.insert("sort_album_artist".into(), v);
    }

    // Credits / personnel
    if let Some(v) = get(ItemKey::Composer) {
        meta.insert("composer".into(), v);
    }
    if let Some(v) = get(ItemKey::Conductor) {
        meta.insert("conductor".into(), v);
    }
    if let Some(v) = get(ItemKey::Lyricist) {
        meta.insert("lyricist".into(), v);
    }
    if let Some(v) = get(ItemKey::Performer) {
        meta.insert("performer".into(), v);
    }
    if let Some(v) = get(ItemKey::Remixer) {
        meta.insert("remixer".into(), v);
    }
    if let Some(v) = get(ItemKey::Label) {
        meta.insert("label".into(), v);
    }
    if let Some(v) = get(ItemKey::Producer) {
        meta.insert("producer".into(), v);
    }

    // Descriptive
    if let Some(v) = get(ItemKey::Bpm) {
        meta.insert("bpm".into(), v);
    }
    if let Some(v) = get(ItemKey::Mood) {
        meta.insert("mood".into(), v);
    }
    if let Some(v) = get(ItemKey::ContentGroup) {
        meta.insert("grouping".into(), v);
    }
    if let Some(v) = get(ItemKey::FlagCompilation) {
        meta.insert("compilation".into(), v);
    }
    if let Some(v) = tag.comment().map(|s| s.to_string()) {
        meta.insert("comment".into(), v);
    }
    if let Some(v) = get(ItemKey::Lyrics) {
        meta.insert("lyrics".into(), v);
    }

    // Identifiers
    if let Some(v) = get(ItemKey::Isrc) {
        meta.insert("isrc".into(), v);
    }
    if let Some(v) = get(ItemKey::Barcode) {
        meta.insert("barcode".into(), v);
    }
    if let Some(v) = get(ItemKey::CatalogNumber) {
        meta.insert("catalog_number".into(), v);
    }
    if let Some(v) = get(ItemKey::OriginalMediaType) {
        meta.insert("media_type".into(), v);
    }

    // Dates
    if let Some(v) = get(ItemKey::ReleaseDate) {
        meta.insert("release_date".into(), v);
    }
    if let Some(v) = get(ItemKey::OriginalReleaseDate) {
        meta.insert("original_date".into(), v);
    }

    // Technical
    if let Some(v) = get(ItemKey::EncodedBy) {
        meta.insert("encoder".into(), v);
    }
    if let Some(v) = get(ItemKey::CopyrightMessage) {
        meta.insert("copyright".into(), v);
    }
    if let Some(v) = get(ItemKey::Language) {
        meta.insert("language".into(), v);
    }

    // ReplayGain
    if let Some(v) = get(ItemKey::ReplayGainTrackGain) {
        meta.insert("rg_track_gain".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainTrackPeak) {
        meta.insert("rg_track_peak".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainAlbumGain) {
        meta.insert("rg_album_gain".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainAlbumPeak) {
        meta.insert("rg_album_peak".into(), v);
    }

    // MusicBrainz IDs
    if let Some(v) = get(ItemKey::MusicBrainzRecordingId) {
        meta.insert("mb_track_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseId) {
        meta.insert("mb_release_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzArtistId) {
        meta.insert("mb_artist_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseArtistId) {
        meta.insert("mb_release_artist_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseGroupId) {
        meta.insert("mb_release_group_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzWorkId) {
        meta.insert("mb_work_id".into(), v);
    }

    meta
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
    use lofty::tag::items::Timestamp;
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
        tag.set_date(Timestamp {
            year: v as u16,
            ..Default::default()
        });
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

    if let Some(composer) = tag.get_string(ItemKey::Composer) {
        credits.push(TrackCredit {
            name: composer.to_string(),
            role: "composer".into(),
            instrument: None,
        });
    }

    if let Some(conductor) = tag.get_string(ItemKey::Conductor) {
        credits.push(TrackCredit {
            name: conductor.to_string(),
            role: "conductor".into(),
            instrument: None,
        });
    }

    if let Some(lyricist) = tag.get_string(ItemKey::Lyricist) {
        credits.push(TrackCredit {
            name: lyricist.to_string(),
            role: "lyricist".into(),
            instrument: None,
        });
    }

    for item in tag.items() {
        if item.key() == ItemKey::Performer
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

/// Helper: build a minimal DSF file with the given audio properties
/// and optional ID3v2 tag appended.
#[cfg(test)]
fn build_dsf_bytes(id3v2_tag: Option<&[u8]>) -> Vec<u8> {
    let metadata_offset: u64 = if id3v2_tag.is_some() { 92 } else { 0 };
    let id3_len = id3v2_tag.map(|t| t.len()).unwrap_or(0);
    let total_size: u64 = 92 + id3_len as u64;

    let mut buf = vec![0u8; 92];
    // DSD chunk (28 bytes)
    buf[0..4].copy_from_slice(b"DSD ");
    buf[4..12].copy_from_slice(&28u64.to_le_bytes());
    buf[12..20].copy_from_slice(&total_size.to_le_bytes());
    buf[20..28].copy_from_slice(&metadata_offset.to_le_bytes());
    // fmt chunk (64 bytes)
    buf[28..32].copy_from_slice(b"fmt ");
    buf[32..40].copy_from_slice(&52u64.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes()); // version
    buf[44..48].copy_from_slice(&0u32.to_le_bytes()); // format ID
    buf[48..52].copy_from_slice(&2u32.to_le_bytes()); // channel type
    buf[52..56].copy_from_slice(&2u32.to_le_bytes()); // channel count = 2
    buf[56..60].copy_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
    buf[60..64].copy_from_slice(&1u32.to_le_bytes()); // bits per sample
    let samples: u64 = 2_822_400 * 180; // 3 minutes
    buf[64..72].copy_from_slice(&samples.to_le_bytes());

    if let Some(tag) = id3v2_tag {
        buf.extend_from_slice(tag);
    }
    buf
}

/// Helper: build a minimal ID3v2.3 tag with the given text frames.
/// Each entry is (frame_id, text_value), using ISO-8859-1 encoding.
#[cfg(test)]
fn build_id3v2_tag(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut frame_bytes = Vec::new();
    for (id, text) in frames {
        assert_eq!(id.len(), 4);
        // Frame header: 4-byte ID + 4-byte size (big-endian) + 2-byte flags
        // Frame data: 1-byte encoding (0 = ISO-8859-1) + text bytes
        let text_bytes = text.as_bytes();
        let frame_size = 1 + text_bytes.len(); // encoding byte + text
        frame_bytes.extend_from_slice(id.as_bytes());
        frame_bytes.extend_from_slice(&(frame_size as u32).to_be_bytes());
        frame_bytes.extend_from_slice(&[0u8; 2]); // flags
        frame_bytes.push(0); // encoding = ISO-8859-1
        frame_bytes.extend_from_slice(text_bytes);
    }

    let tag_size = frame_bytes.len();
    // Encode tag_size as syncsafe integer
    let ss = [
        ((tag_size >> 21) & 0x7F) as u8,
        ((tag_size >> 14) & 0x7F) as u8,
        ((tag_size >> 7) & 0x7F) as u8,
        (tag_size & 0x7F) as u8,
    ];

    let mut tag = Vec::new();
    tag.extend_from_slice(b"ID3");
    tag.push(3); // version major = ID3v2.3
    tag.push(0); // version minor
    tag.push(0); // flags
    tag.extend_from_slice(&ss);
    tag.extend_from_slice(&frame_bytes);
    tag
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
        assert_eq!(genres, vec!["Musique Classique", "Musique Experimentale"]);
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
        assert_eq!(normalize_format("mpeg", None), "mp3");
    }

    #[test]
    fn normalize_format_dsf_to_dsd() {
        assert_eq!(normalize_format("dsf", None), "dsd");
    }

    #[test]
    fn normalize_format_dff_to_dsd() {
        assert_eq!(normalize_format("dff", None), "dsd");
    }

    #[test]
    fn normalize_format_flac_unchanged() {
        assert_eq!(normalize_format("flac", None), "flac");
    }

    #[test]
    fn normalize_format_wav_unchanged() {
        assert_eq!(normalize_format("wav", None), "wav");
    }

    #[test]
    fn normalize_format_aiff_unchanged() {
        assert_eq!(normalize_format("aiff", None), "aiff");
    }

    #[test]
    fn dsf_dff_fallback_returns_none_for_non_dsd() {
        assert!(dsf_dff_fallback(Path::new("/tmp/test.flac")).is_none());
        assert!(dsf_dff_fallback(Path::new("/tmp/test.mp3")).is_none());
    }

    #[test]
    fn dsf_dff_fallback_returns_dsd_format() {
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
        let tmp = std::env::temp_dir().join("tune_test_dsf_fallback.dsf");
        let buf = build_dsf_bytes(None);
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
        let dur = meta.duration_ms.unwrap();
        assert!(
            (179_000..=181_000).contains(&dur),
            "unexpected duration: {dur}ms"
        );
    }

    #[test]
    fn dsf_fallback_reads_id3v2_tags() {
        use std::io::Write;
        let id3_tag = build_id3v2_tag(&[
            ("TIT2", "Man On The Corner"),
            ("TPE1", "Genesis"),
            ("TALB", "Abacab"),
            ("TPE2", "Genesis"),
            ("TRCK", "7/11"),
            ("TPOS", "1/2"),
            ("TDRC", "1981"),
            ("TCON", "Rock"),
            ("TPUB", "Virgin Records"),
        ]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        let tmp = std::env::temp_dir().join("tune_test_dsf_id3v2.dsf");
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = dsf_dff_fallback(&tmp);
        std::fs::remove_file(&tmp).ok();
        assert!(
            meta.is_some(),
            "dsf_dff_fallback should return Some for DSF with ID3v2"
        );
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
        assert_eq!(meta.artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.album.as_deref(), Some("Abacab"));
        assert_eq!(meta.album_artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.track_number, Some(7));
        assert_eq!(meta.total_tracks, Some(11));
        assert_eq!(meta.disc_number, Some(1));
        assert_eq!(meta.total_discs, Some(2));
        assert_eq!(meta.year, Some(1981));
        assert_eq!(meta.genre.as_deref(), Some("Rock"));
        assert_eq!(meta.label.as_deref(), Some("Virgin Records"));
        assert_eq!(meta.format.as_deref(), Some("dsd"));
        assert_eq!(meta.sample_rate, Some(2_822_400));
        assert_eq!(meta.channels, Some(2));
        assert_eq!(meta.bit_depth, Some(1));
    }

    #[test]
    fn dsf_fallback_id3v2_overrides_path() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("V_DSF").join("Genesis - Abacab");
        std::fs::create_dir_all(&dir).ok();
        let file_path = dir.join("07 - Man On The Corner.dsf");
        let id3_tag = build_id3v2_tag(&[
            ("TIT2", "Man On The Corner"),
            ("TPE1", "Genesis"),
            ("TALB", "Abacab"),
            ("TRCK", "7"),
        ]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = dsf_dff_fallback(&file_path);
        std::fs::remove_file(&file_path).ok();
        std::fs::remove_dir_all(std::env::temp_dir().join("V_DSF")).ok();
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
        assert_eq!(meta.artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.album.as_deref(), Some("Abacab"));
        assert_eq!(meta.track_number, Some(7));
    }

    #[test]
    fn try_read_metadata_dsf_fallback() {
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.dsf"));
        assert!(result.is_ok());
        let meta = result.unwrap();
        assert_eq!(meta.format.as_deref(), Some("dsd"));
    }

    #[test]
    fn try_read_metadata_non_dsd_still_errors() {
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.flac"));
        assert!(result.is_err());
    }

    #[test]
    fn normalize_format_mp4_aac_no_bit_depth() {
        // AAC (lossy) in M4A container: lofty reports no bit depth
        assert_eq!(normalize_format("mp4", None), "aac");
        assert_eq!(normalize_format("m4a", None), "aac");
    }

    #[test]
    fn normalize_format_mp4_alac_with_bit_depth() {
        // ALAC (lossless) in M4A container: lofty reports bit depth (16 or 24)
        assert_eq!(normalize_format("mp4", Some(16)), "alac");
        assert_eq!(normalize_format("mp4", Some(24)), "alac");
        assert_eq!(normalize_format("m4a", Some(16)), "alac");
        assert_eq!(normalize_format("m4a", Some(24)), "alac");
    }

    #[test]
    fn normalize_format_unknown_passthrough() {
        assert_eq!(normalize_format("ogg", None), "ogg");
        assert_eq!(normalize_format("opus", None), "opus");
        assert_eq!(normalize_format("wv", None), "wv");
        assert_eq!(normalize_format("ape", None), "ape");
    }

    #[test]
    fn split_genre_parenthesized_id3v1_numeric() {
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
        assert_eq!(normalize_format("mpeg", None), "mp3");
        assert_eq!(normalize_format("MPEG", None), "MPEG");
    }

    #[test]
    fn normalize_genre_title_case() {
        assert_eq!(normalize_genre("classique"), "Classique");
        assert_eq!(normalize_genre("ROCK"), "Rock");
        assert_eq!(normalize_genre("jazz"), "Jazz");
        assert_eq!(normalize_genre("Jazz"), "Jazz");
    }

    #[test]
    fn normalize_genre_multi_word() {
        assert_eq!(normalize_genre("hip hop"), "Hip Hop");
        assert_eq!(normalize_genre("trip hop"), "Trip Hop");
        assert_eq!(normalize_genre("HARD ROCK"), "Hard Rock");
    }

    #[test]
    fn normalize_genre_special_tokens() {
        assert_eq!(normalize_genre("r&b"), "R&B");
        assert_eq!(normalize_genre("R&B"), "R&B");
        assert_eq!(normalize_genre("dj mix"), "DJ Mix");
        assert_eq!(normalize_genre("DJ"), "DJ");
        assert_eq!(normalize_genre("edm"), "EDM");
        assert_eq!(normalize_genre("uk garage"), "UK Garage");
    }

    #[test]
    fn normalize_genre_slash_compound() {
        assert_eq!(normalize_genre("Folk/Rock"), "Folk/Rock");
        assert_eq!(normalize_genre("folk/rock"), "Folk/Rock");
        assert_eq!(normalize_genre("FOLK/ROCK"), "Folk/Rock");
    }

    #[test]
    fn normalize_genre_already_correct() {
        assert_eq!(normalize_genre("Progressive Rock"), "Progressive Rock");
        assert_eq!(normalize_genre("Jazz"), "Jazz");
    }

    #[test]
    fn split_genre_normalizes_case() {
        let genres = split_genre_tag("classique; ROCK; jazz");
        assert_eq!(genres, vec!["Classique", "Rock", "Jazz"]);
    }

    #[test]
    fn parse_id3v2_basic_text_frames() {
        let tag_bytes = build_id3v2_tag(&[
            ("TIT2", "Test Title"),
            ("TPE1", "Test Artist"),
            ("TALB", "Test Album"),
        ]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.title(), Some("Test Title"));
        assert_eq!(tags.artist(), Some("Test Artist"));
        assert_eq!(tags.album(), Some("Test Album"));
    }

    #[test]
    fn parse_id3v2_track_disc_parsing() {
        let tag_bytes = build_id3v2_tag(&[("TRCK", "7/11"), ("TPOS", "2/3")]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.track_number(), Some(7));
        assert_eq!(tags.total_tracks(), Some(11));
        assert_eq!(tags.disc_number(), Some(2));
        assert_eq!(tags.total_discs(), Some(3));
    }

    #[test]
    fn parse_id3v2_year_from_tdrc() {
        let tag_bytes = build_id3v2_tag(&[("TDRC", "1981")]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.year(), Some(1981));
    }

    #[test]
    fn parse_id3v2_invalid_magic() {
        assert!(parse_id3v2_tag(b"NOT_ID3_").is_none());
    }

    #[test]
    fn syncsafe_integer_values() {
        assert_eq!(syncsafe_to_u32(&[0, 0, 0, 127]), 127);
        assert_eq!(syncsafe_to_u32(&[0, 0, 1, 0]), 128);
        assert_eq!(syncsafe_to_u32(&[0, 0, 2, 0]), 256);
    }
}
