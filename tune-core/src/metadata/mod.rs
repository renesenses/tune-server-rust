pub mod artist_enrichment;
pub mod artist_split;
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

/// Build the MusicBrainz Lucene clause used to look up an artist by name.
///
/// A bare `artist:"<name>"` phrase only matches the artist's primary `name`
/// (and `sort-name`). For non-Latin artists MusicBrainz stores the romanized
/// form as the primary name (e.g. `IU`, `BTS`, `坂本龍一`→`Ryuichi Sakamoto`)
/// and keeps the native-script name only as an *alias*. The bare phrase query
/// therefore returns zero results for a Hangul/CJK/Cyrillic query, so no MBID
/// is resolved and no bio/image enrichment happens.
///
/// Adding `OR alias:"<name>"` makes the native-script name resolve while
/// keeping the quoted phrase precision for Latin names (verified against the
/// live MB API on IU/BTS/坂本龍一 as well as Radiohead/The Beatles/Björk).
pub(crate) fn mb_artist_query(name: &str) -> String {
    format!("artist:\"{name}\" OR alias:\"{name}\"")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackCredit {
    pub name: String,
    pub role: String,
    pub instrument: Option<String>,
}

/// Max size of an embedded cover kept in `TrackMetadata.cover_art`. The scanner
/// retains this buffer for every file and accumulates a whole batch in memory,
/// so an oversized (or malformed) embedded picture, multiplied across files,
/// blew the scanner past the OOM killer (JeromeQ: 261 files → 6.1 GB RSS on an
/// 8 GB machine). Above this, we keep `has_cover=true` but drop the bytes and
/// let the scan re-extract that one file's cover to the artwork cache on demand,
/// keeping peak scan memory bounded. Normal covers (well under 4 MB) stay cached.
pub const MAX_RETAINED_COVER_BYTES: usize = 4 * 1024 * 1024;

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
    /// Embedded cover art (bytes, mime) read from the SAME lofty pass that
    /// parsed the tags. Lets the scanner cache the cover without re-opening the
    /// file — a second `lofty::read_from_path` failed with "path not found"
    /// (os error 3) for some accented Windows paths even though the first read
    /// succeeded (Thibaud: <1% of albums had no artwork).
    pub cover_art: Option<(Vec<u8>, String)>,
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

    // Handle slash-separated compound genres like "Folk/Rock", and title-case
    // each hyphen-separated part so "Folk-Punk" stays "Folk-Punk" (not
    // "Folk-punk") and "Hip-Hop"/"Lo-Fi" keep both parts capitalised
    // (Yves Scordia: Folk-Punk was lower-cased after the hyphen).
    genre
        .split('/')
        .map(|part| {
            part.split_whitespace()
                .map(|word| {
                    word.split('-')
                        .map(title_case_word)
                        .collect::<Vec<_>>()
                        .join("-")
                })
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

    let file = std::fs::File::open(&*crate::library::artwork::extended_path(path)).ok()?;
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

/// Probe an M4A/MP4 file for its real codec **and** bit depth.
///
/// lofty reports neither for these files: it can't tell ALAC (lossless) from
/// AAC (lossy) and never fills in the bit depth. symphonia's ISOMP4 demuxer
/// also leaves `bits_per_sample` empty for ALAC, so the depth is read from the
/// ALAC magic cookie in `extra_data` (bit depth at byte 5 of the 24-byte
/// payload, after optional `frma`/`alac` atom prefixes) — the same layout the
/// decoder uses. Returns `(format, bit_depth)`; bit depth is `None` for AAC.
pub fn probe_m4a_props(path: &std::path::Path) -> Option<(String, Option<u16>)> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(&*crate::library::artwork::extended_path(path)).ok()?;
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

    // Match on the codec id (0x2003 for ALAC) rather than a Debug string — the
    // Debug form of the codec parameters doesn't spell out "Alac".
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return Some(("aac".to_string(), None)),
    };
    if params.codec != CODEC_ID_ALAC {
        return Some(("aac".to_string(), None));
    }

    let bit_depth = params
        .bits_per_sample
        .map(|b| b as u16)
        .or_else(|| alac_bit_depth_from_cookie(params.extra_data.as_deref()));
    Some(("alac".to_string(), bit_depth))
}

/// Extract the ALAC bit depth from the magic cookie (`extra_data`).
/// Byte 5 of the 24-byte payload holds the bit depth, after optional 12-byte
/// `frma` and `alac` atom prefixes.
fn alac_bit_depth_from_cookie(extra: Option<&[u8]>) -> Option<u16> {
    let mut buf = extra?;
    if buf.len() >= 12 && &buf[4..8] == b"frma" {
        buf = &buf[12..];
    }
    if buf.len() >= 12 && &buf[4..8] == b"alac" {
        buf = &buf[12..];
    }
    if buf.len() >= 24 {
        let bd = buf[5];
        if bd > 0 && bd <= 32 {
            return Some(bd as u16);
        }
    }
    None
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

    let mut f =
        std::fs::File::open(&*crate::library::artwork::extended_path(path)).map_err(|_| ())?;
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
    /// First embedded picture found, as `(mime_type, image_bytes)`.
    picture: Option<(String, Vec<u8>)>,
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

/// Reverse ID3v2 unsynchronisation: every `0xFF 0x00` pair becomes `0xFF`.
/// Applied to the whole tag body when the header's unsynchronisation flag
/// (0x80) is set (ID3v2.2/v2.3). A no-op when no such pair is present. Old
/// taggers commonly set this on DSD/DSF files (Benjithom, #959); without
/// reversing it the frame sizes desync and the title/artist are lost.
fn deunsynchronise(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        out.push(data[i]);
        if data[i] == 0xFF && i + 1 < data.len() && data[i + 1] == 0x00 {
            i += 2; // drop the stuffed 0x00
        } else {
            i += 1;
        }
    }
    out
}

/// Read and parse an ID3v2 tag from a byte slice starting at "ID3".
///
/// Supports ID3v2.3 and ID3v2.4 text frames (TIT2, TPE1, TALB, etc.)
/// and TXXX user-defined text frames. Skips binary frames (APIC, etc.)
/// but notes their presence.
/// Map an ID3v2.2 three-character frame id to its v2.3/v2.4 four-character
/// equivalent so the rest of the reader (which keys on `TIT2`, `TPE1`, …) works.
/// DSD/DSF files are frequently tagged with ID3v2.2 (Benjithom: the title showed
/// as the filename because v2.2 was skipped entirely).
fn map_id3v22_frame(id: &str) -> Option<&'static str> {
    Some(match id {
        "TT2" => "TIT2", // title
        "TT1" => "TIT1",
        "TT3" => "TIT3",
        "TP1" => "TPE1", // artist
        "TP2" => "TPE2", // album artist
        "TP3" => "TPE3",
        "TAL" => "TALB", // album
        "TRK" => "TRCK", // track number
        "TPA" => "TPOS", // disc number
        "TYE" => "TYER", // year
        "TCO" => "TCON", // genre
        "TCM" => "TCOM", // composer
        "TCP" => "TCMP", // compilation flag
        "TOR" => "TORY",
        "TDA" => "TDAT",
        "TXX" => "TXXX", // user-defined text
        "PIC" => "APIC", // attached picture
        _ => return None,
    })
}

fn parse_id3v2_tag(data: &[u8]) -> Option<Id3v2Tags> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let major_version = data[3]; // 2 = ID3v2.2, 3 = ID3v2.3, 4 = ID3v2.4
    let _minor_version = data[4];
    let flags = data[5];
    let tag_size = syncsafe_to_u32(&data[6..10]) as usize;

    // We handle ID3v2.2, v2.3 and v2.4.
    if major_version < 2 || major_version > 4 {
        return None;
    }

    // Extended header (v2.3/v2.4 only — in v2.2 that flag bit means compression).
    let mut pos = 10;
    if major_version >= 3 && flags & 0x40 != 0 {
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

    // ID3v2.2/v2.3 may unsynchronise the whole tag (header flag 0x80): every
    // 0xFF byte is followed by a stuffed 0x00 that must be removed before the
    // frames can be parsed. Old taggers commonly set this on DSD/DSF files
    // (Benjithom, #959) — without reversing it the frame sizes desync (notably
    // when a PIC image precedes the title) and the title is lost, so Tune fell
    // back to the filename. (v2.4 uses per-frame unsync, not handled here.)
    let unsync = flags & 0x80 != 0;
    let raw_frames = &data[pos.min(tag_end)..tag_end];
    let deunsynced;
    let frames: &[u8] = if unsync && major_version <= 3 {
        deunsynced = deunsynchronise(raw_frames);
        &deunsynced
    } else {
        raw_frames
    };
    let frames_end = frames.len();

    // v2.2 frames: 3-char id + 3-byte size, no flags (6-byte header).
    // v2.3/v2.4 frames: 4-char id + 4-byte size + 2-byte flags (10-byte header).
    let (id_len, header_len) = if major_version == 2 { (3, 6) } else { (4, 10) };

    let mut fpos = 0usize;
    while fpos + header_len <= frames_end {
        let raw_id = match std::str::from_utf8(&frames[fpos..fpos + id_len]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };

        // Stop on padding (null bytes)
        if raw_id.starts_with('\0') {
            break;
        }

        let frame_size = match major_version {
            4 => syncsafe_to_u32(&frames[fpos + 4..fpos + 8]) as usize,
            3 => u32::from_be_bytes([
                frames[fpos + 4],
                frames[fpos + 5],
                frames[fpos + 6],
                frames[fpos + 7],
            ]) as usize,
            // v2.2: 3-byte big-endian size.
            _ => {
                ((frames[fpos + 3] as usize) << 16)
                    | ((frames[fpos + 4] as usize) << 8)
                    | (frames[fpos + 5] as usize)
            }
        };

        fpos += header_len; // skip frame header

        // Normalize v2.2 3-char ids to their v2.3/v2.4 equivalents.
        let frame_id = if major_version == 2 {
            map_id3v22_frame(&raw_id)
                .map(|s| s.to_string())
                .unwrap_or(raw_id)
        } else {
            raw_id
        };

        if frame_size == 0 || fpos + frame_size > frames_end {
            break;
        }

        let frame_data = &frames[fpos..fpos + frame_size];
        fpos += frame_size;

        // Check for picture frames (APIC in v2.3/2.4, PIC in v2.2).
        if frame_id == "APIC" {
            tags.has_picture = true;
            if tags.picture.is_none() {
                tags.picture = extract_apic_picture(frame_data, major_version);
            }
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

/// Read the raw ID3v2 tag bytes from a DSF file's metadata chunk.
///
/// DSF files store an ID3v2 tag at the byte offset specified in the DSD
/// chunk header (bytes 20-27). Returns the tag as a contiguous buffer
/// (ID3v2 header + body), or `None` if there is no tag or it looks invalid.
fn read_dsf_id3v2_raw(path: &Path, metadata_offset: Option<u64>) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let offset = metadata_offset?;

    let mut f = std::fs::File::open(&*crate::library::artwork::extended_path(path)).ok()?;
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

    Some(tag_data)
}

/// Read and parse the ID3v2 metadata chunk from a DSF file.
fn read_dsf_id3v2_tags(path: &Path, metadata_offset: Option<u64>) -> Option<Id3v2Tags> {
    let tag_data = read_dsf_id3v2_raw(path, metadata_offset)?;
    parse_id3v2_tag(&tag_data)
}

/// Decode the image bytes and MIME type from an ID3v2 picture frame body.
///
/// Handles both the v2.3/2.4 `APIC` layout (encoding byte, NUL-terminated
/// Latin-1 MIME string, picture-type byte, NUL-terminated description, image
/// data) and the v2.2 `PIC` layout (encoding byte, 3-char image-format code,
/// picture-type byte, NUL-terminated description, image data). The description
/// terminator is one NUL for Latin-1/UTF-8 encodings and a two-byte NUL for the
/// UTF-16 encodings. Returns `(mime_type, image_bytes)`.
fn extract_apic_picture(body: &[u8], major_version: u8) -> Option<(String, Vec<u8>)> {
    if body.is_empty() {
        return None;
    }
    let encoding = body[0];
    let mut pos = 1usize;

    let mime = if major_version == 2 {
        // v2.2 "PIC": 3-character image format code (e.g. "JPG", "PNG").
        if body.len() < pos + 3 {
            return None;
        }
        let fmt = &body[pos..pos + 3];
        pos += 3;
        match fmt.to_ascii_uppercase().as_slice() {
            b"PNG" => "image/png".to_string(),
            _ => "image/jpeg".to_string(),
        }
    } else {
        // v2.3/2.4 "APIC": NUL-terminated Latin-1 MIME string.
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        if pos >= body.len() {
            return None;
        }
        let mime = String::from_utf8_lossy(&body[start..pos]).into_owned();
        pos += 1; // skip NUL terminator
        if mime.is_empty() {
            "image/jpeg".to_string()
        } else {
            mime
        }
    };

    // Picture type (1 byte).
    if pos >= body.len() {
        return None;
    }
    pos += 1;

    // Description, NUL-terminated in the frame's text encoding.
    match encoding {
        1 | 2 => {
            // UTF-16: terminated by a 0x0000 code unit.
            while pos + 1 < body.len() && !(body[pos] == 0 && body[pos + 1] == 0) {
                pos += 2;
            }
            pos += 2;
        }
        _ => {
            while pos < body.len() && body[pos] != 0 {
                pos += 1;
            }
            pos += 1;
        }
    }

    if pos >= body.len() {
        return None;
    }
    let data = body[pos..].to_vec();
    if data.is_empty() {
        return None;
    }
    Some((mime, data))
}

/// Extract the embedded cover art (APIC) from a DSF file's ID3v2 chunk.
///
/// lofty does not read the ID3v2 tag stored at the DSF metadata offset, so
/// embedded artwork is invisible to the generic `lofty::read_from_path`
/// cover-extraction path used by [`crate::library::artwork::extract_cover_art`].
/// This reads the tag directly and returns the first picture's raw bytes and
/// MIME type. Non-`.dsf` paths (and files without embedded art) return `None`.
pub(crate) fn extract_dsf_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "dsf" {
        return None;
    }

    let info = parse_dsf_header_full(path).ok()?;
    let tag_data = read_dsf_id3v2_raw(path, info.metadata_offset)?;

    let (mime, data) = parse_id3v2_tag(&tag_data)?.picture?;
    Some((data, mime))
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

    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .ok()
        .map(|m| m.len());

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

    // Fall back to filename/directory for fields the ID3v2 tag didn't provide.
    // Treat a present-but-empty/whitespace tag as absent: a file whose ALBUM tag
    // is "" (not missing) otherwise produced a blank, untitled album that no
    // amount of re-scanning could name (Bilou #1093). `filter` drops the empty
    // value so the folder-name fallback kicks in.
    let title = title
        .filter(|s| !s.trim().is_empty())
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()));
    let album = album.filter(|s| !s.trim().is_empty()).or_else(|| {
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
        cover_art: None,
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

    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .ok()
        .map(|m| m.len());

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
        cover_art: None,
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
    let mut probed_bit_depth: Option<u16> = None;
    let format = {
        let mut fmt = normalize_format(&ext, props.bit_depth());
        if fmt == "aac" && (ext == "m4a" || ext == "mp4") && props.bit_depth().is_none() {
            if let Some((probed, bd)) = probe_m4a_props(path) {
                fmt = probed;
                probed_bit_depth = bd;
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
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
        sample_rate: props.sample_rate(),
        channels: props.channels().map(|c| c as u16),
        duration_ms: Some(props.duration().as_millis() as u64),
        bit_depth: props.bit_depth().map(|b| b as u16).or(probed_bit_depth),
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
        cover_art: None,
        credits: vec![],
        comment: None,
    }
}

/// Fallback when lofty cannot parse the file at all (no audio properties).
/// Extracts everything from the filesystem.
/// Path/filename-only metadata (no file I/O). Used as a last resort when the
/// tag reader fails or times out, so a file still appears in the library.
pub fn tagless_fallback_no_props(path: &Path) -> TrackMetadata {
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
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
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
        cover_art: None,
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
    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .map(|m| m.len())
        .unwrap_or(0);
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
                .max_junk_bytes(1024 * 1024)
                // Don't load embedded cover art in the tag pass: lofty otherwise
                // reads the whole PICTURE block into memory, and a huge/malformed
                // embedded image, multiplied by the scan's concurrency (up to 32
                // reads at once), spikes the scanner past the OOM killer (JeromeQ:
                // 261 files → 6.1 GB RSS → tune-server killed, black screen). The
                // cover is extracted separately, sequentially, by
                // `artwork::get_or_extract` when the album needs one, so artwork
                // is unaffected. (has_cover becomes false here — it has no
                // consumers beyond serialization; the album cover_path is the
                // real signal.)
                .read_cover_art(false),
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

    // DSF/DFF: lofty parses the container and returns a tag object, but often
    // misreads the ID3v2.2 frames commonly used on DSD files — the title comes
    // back empty and the track ends up showing its filename (LANDES Philippe,
    // Benjithom). Because a (mostly-empty) tag *is* present, the `None` branch
    // above never fires. So when lofty's title is empty for a DSD file, prefer
    // our own ID3v2.2/.3/.4 parser, which reads those frames correctly.
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if matches!(ext.as_str(), "dsf" | "dff")
            && tag.title().map_or(true, |t| t.trim().is_empty())
        {
            if let Some(meta) = dsf_dff_fallback(path) {
                if meta
                    .title
                    .as_deref()
                    .map_or(false, |t| !t.trim().is_empty())
                {
                    return Ok(meta);
                }
            }
        }
    }

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

    // lofty can't distinguish ALAC (lossless) from AAC (lossy) in an M4A/MP4
    // container and reports no bit depth for either, so a tagged ALAC file was
    // stored as "aac" with no bit depth — the signal path then showed the wrong
    // format and a fabricated 16-bit (Yves: ALAC 24/96 shown as AAC/FLAC).
    // Probe the real codec and, for ALAC, the true bit depth from the magic
    // cookie. Only used for M4A containers with no lofty bit depth.
    let file_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let m4a_probe = if (file_ext == "m4a" || file_ext == "mp4" || file_ext == "m4b")
        && props.bit_depth().is_none()
    {
        probe_m4a_props(path)
    } else {
        None
    };

    // lofty occasionally mis-decodes an MP3's ID3v2 text frames and returns an
    // EMPTY title/artist/album even though the frames are valid (Yves Scordia: a
    // Chris Isaak MP3 with UTF-16 TIT2/TPE1/TALB read as empty, so BluOS got no
    // metadata — other frames like TPE2/TCON/TYER read fine). When the title is
    // empty and the file has a leading ID3v2 tag (MP3, WAV+ID3), re-read those
    // frames with our own ID3v2 parser — the same one used for DSF.
    let mut title = tag.title().map(|s| s.to_string());
    let mut artist = tag.artist().map(|s| s.to_string());
    let mut album = tag.album().map(|s| s.to_string());
    if title.as_deref().map_or(true, |t| t.trim().is_empty()) {
        if let Some(raw) = read_dsf_id3v2_raw(path, Some(0)) {
            if let Some(id3) = parse_id3v2_tag(&raw) {
                let prefer = |cur: Option<String>, alt: Option<&str>| -> Option<String> {
                    if cur.as_deref().map_or(true, |x| x.trim().is_empty()) {
                        alt.filter(|s| !s.trim().is_empty())
                            .map(|s| s.to_string())
                            .or(cur)
                    } else {
                        cur
                    }
                };
                title = prefer(title, id3.title());
                artist = prefer(artist, id3.artist());
                album = prefer(album, id3.album());
            }
        }
    }

    Ok(TrackMetadata {
        title,
        artist,
        album,
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
        bit_depth: props
            .bit_depth()
            .map(|b| b as u16)
            .or_else(|| m4a_probe.as_ref().and_then(|(_, bd)| *bd)),
        channels: props.channels().map(|c| c as u16),
        format: Some(match m4a_probe.as_ref() {
            Some((fmt, _)) => fmt.clone(),
            None => normalize_format(
                &format!("{:?}", tagged.file_type()).to_lowercase(),
                props.bit_depth(),
            ),
        }),
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
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
        // Capture the embedded cover from this same lofty pass so the scanner
        // doesn't have to re-open the file to extract it.
        cover_art: tag.pictures().first().and_then(|pic| {
            let data = pic.data();
            // Don't retain oversized embedded pictures — they accumulate across
            // the scan batch and OOM the scanner. has_cover stays true, so the
            // scan re-extracts this file's cover to the cache on demand.
            if data.len() > MAX_RETAINED_COVER_BYTES {
                return None;
            }
            let mime = match pic.mime_type() {
                Some(lofty::picture::MimeType::Png) => "image/png",
                Some(lofty::picture::MimeType::Bmp) => "image/bmp",
                _ => "image/jpeg",
            };
            Some((data.to_vec(), mime.to_string()))
        }),
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
                .max_junk_bytes(1024 * 1024)
                // Don't load embedded cover art: this pass only reads text tags
                // (sort orders, credits, ISRC, lyrics…) via get_string and never
                // touches the picture. Without this, lofty reads the whole PICTURE
                // block into memory for EVERY file the scanner processes (called
                // per file in auto_scan's batch callback), and a huge/malformed
                // embedded image spikes RSS past the OOM killer — the same failure
                // try_read_metadata was hardened against (#JeromeQ), which this
                // second read path was missing (.15: 31 115 new files → ~14 GB RSS
                // → OOM crash-loop). Cover extraction stays in artwork::get_or_extract.
                .read_cover_art(false),
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
    fn mb_artist_query_includes_alias_clause() {
        // The alias clause is what lets non-Latin (Hangul/CJK) names resolve:
        // MusicBrainz indexes their romanized form as `name` and the native
        // script only as an alias, so a bare `artist:"…"` phrase returns none.
        let q = mb_artist_query("아이유");
        assert_eq!(q, "artist:\"아이유\" OR alias:\"아이유\"");
        assert!(q.contains("alias:"));
        // Quoted phrase precision preserved for Latin names.
        assert_eq!(
            mb_artist_query("The Beatles"),
            "artist:\"The Beatles\" OR alias:\"The Beatles\""
        );
    }

    /// Build a minimal ID3v2.3 tag containing a single APIC frame.
    fn id3v23_with_apic(mime: &[u8], img: &[u8]) -> Vec<u8> {
        // Frame body: encoding(1) + mime + NUL + pic_type(1) + desc NUL + data.
        let mut body = vec![0u8]; // Latin-1
        body.extend_from_slice(mime);
        body.push(0);
        body.push(3); // picture type: front cover
        body.push(0); // empty description + NUL
        body.extend_from_slice(img);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
        // syncsafe tag size (frame header 10 + body)
        let size = (10 + body.len()) as u32;
        tag.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        tag.extend_from_slice(b"APIC");
        tag.extend_from_slice(&(body.len() as u32).to_be_bytes()); // v2.3 plain size
        tag.extend_from_slice(&[0, 0]); // frame flags
        tag.extend_from_slice(&body);
        tag
    }

    #[test]
    fn apic_extracted_from_id3v23() {
        let img = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]; // JPEG-ish
        let tag = id3v23_with_apic(b"image/jpeg", &img);
        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert!(parsed.has_picture);
        let (mime, data) = parsed.picture.expect("picture present");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, img);
    }

    #[test]
    fn apic_body_v22_pic_png() {
        // v2.2 "PIC": encoding(1) + 3-char format + pic_type(1) + desc NUL + data.
        let img = [0x89u8, 0x50, 0x4E, 0x47, 9, 9];
        let mut body = vec![0u8]; // Latin-1
        body.extend_from_slice(b"PNG");
        body.push(3); // picture type
        body.push(0); // empty description
        body.extend_from_slice(&img);
        let (mime, data) = extract_apic_picture(&body, 2).expect("v2.2 PIC parses");
        assert_eq!(mime, "image/png");
        assert_eq!(data, img);
    }

    #[test]
    fn deunsynchronise_removes_stuffed_zeros() {
        // 0xFF 0x00 -> 0xFF; other bytes untouched; trailing 0xFF kept.
        assert_eq!(
            deunsynchronise(&[0x01, 0xFF, 0x00, 0x02, 0xFF, 0x00, 0xFF]),
            vec![0x01, 0xFF, 0x02, 0xFF, 0xFF]
        );
        // No stuffing -> identity.
        assert_eq!(deunsynchronise(&[0x01, 0x02, 0x03]), vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn parse_id3v23_utf16_title() {
        // A v2.3 TIT2 frame encoded as UTF-16-with-BOM — the case lofty
        // mis-decoded as an empty string on Yves Scordia's Chris Isaak MP3, so
        // Tune now falls back to this parser. Verify it reads the real title.
        fn utf16_frame(id: &str, text: &str) -> Vec<u8> {
            let mut body = vec![0x01u8]; // encoding 1 = UTF-16 with BOM
            body.extend_from_slice(&[0xFF, 0xFE]); // little-endian BOM
            for u in text.encode_utf16() {
                body.extend_from_slice(&u.to_le_bytes());
            }
            let mut f = id.as_bytes().to_vec();
            f.extend_from_slice(&(body.len() as u32).to_be_bytes()); // v2.3 plain size
            f.extend_from_slice(&[0, 0]); // frame flags
            f.extend_from_slice(&body);
            f
        }
        let mut frames = utf16_frame("TIT2", "First Comes The Night");
        frames.extend_from_slice(&utf16_frame("TPE1", "Chris Isaak"));
        let mut tag = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00]; // ID3v2.3, no flags
        let size = frames.len();
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&frames);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("First Comes The Night"));
        assert_eq!(parsed.artist(), Some("Chris Isaak"));
    }

    #[test]
    fn parse_id3v22_unsynchronised_title() {
        // An unsynchronised ID3v2.2 tag (header flag 0x80) with a PIC frame
        // whose data contains 0xFF bytes — which get 0x00-stuffed — placed
        // BEFORE the TT2 title. Without de-unsynchronisation the PIC frame's
        // real byte length exceeds its declared size, the cursor desyncs and
        // the title is never found → filename fallback (Benjithom, #959).
        fn frame_v22(id: &str, data: &[u8]) -> Vec<u8> {
            let mut f = id.as_bytes().to_vec();
            let n = data.len();
            f.push((n >> 16) as u8);
            f.push((n >> 8) as u8);
            f.push(n as u8);
            f.extend_from_slice(data);
            f
        }
        // Frame sizes are the de-synchronised (true) sizes.
        let pic = frame_v22("PIC", &[0x00, 0xFF, 0xFF, 0x01]);
        let tt2 = frame_v22("TT2", &[0x00, b'H', b'i']); // Latin-1 "Hi"
        let mut body = Vec::new();
        body.extend_from_slice(&pic);
        body.extend_from_slice(&tt2);

        // Unsynchronise the assembled body: 0xFF -> 0xFF 0x00.
        let mut unsynced = Vec::new();
        for &b in &body {
            unsynced.push(b);
            if b == 0xFF {
                unsynced.push(0x00);
            }
        }

        let size = unsynced.len();
        let mut tag = vec![b'I', b'D', b'3', 0x02, 0x00, 0x80]; // v2.2, unsync flag
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&unsynced);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("Hi"));
        assert!(parsed.has_picture);
    }

    #[test]
    fn apic_utf16_description_skipped() {
        // encoding 1 (UTF-16): description terminated by a 2-byte NUL.
        let img = [0xFFu8, 0xD8, 42];
        let mut body = vec![1u8]; // UTF-16
        body.extend_from_slice(b"image/jpeg");
        body.push(0);
        body.push(3); // picture type
        body.extend_from_slice(&[0x00, 0x00]); // empty UTF-16 description
        body.extend_from_slice(&img);
        let (mime, data) = extract_apic_picture(&body, 4).expect("utf-16 desc parses");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, img);
    }

    #[test]
    fn alac_bit_depth_from_magic_cookie() {
        // ALACSpecificConfig: bit depth lives at byte 5 of the 24-byte payload.
        let mut cookie = [0u8; 24];
        cookie[5] = 24;
        assert_eq!(alac_bit_depth_from_cookie(Some(&cookie)), Some(24));
        cookie[5] = 16;
        assert_eq!(alac_bit_depth_from_cookie(Some(&cookie)), Some(16));

        // With the optional `frma`/`alac` atom prefixes, the payload is offset.
        let mut prefixed = Vec::new();
        prefixed.extend_from_slice(&[0, 0, 0, 12]);
        prefixed.extend_from_slice(b"frma");
        prefixed.extend_from_slice(&[0, 0, 0, 0]);
        prefixed.extend_from_slice(&[0, 0, 0, 12]);
        prefixed.extend_from_slice(b"alac");
        prefixed.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = [0u8; 24];
        payload[5] = 24;
        prefixed.extend_from_slice(&payload);
        assert_eq!(alac_bit_depth_from_cookie(Some(&prefixed)), Some(24));

        // Missing / too-short / out-of-range depths yield None.
        assert_eq!(alac_bit_depth_from_cookie(None), None);
        assert_eq!(alac_bit_depth_from_cookie(Some(&[0u8; 10])), None);
        let mut bad = [0u8; 24];
        bad[5] = 99;
        assert_eq!(alac_bit_depth_from_cookie(Some(&bad)), None);
    }

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
    fn normalize_genre_preserves_hyphen_casing() {
        // Yves Scordia: "Folk-Punk" was lower-cased after the hyphen.
        assert_eq!(normalize_genre("Folk-Punk"), "Folk-Punk");
        assert_eq!(normalize_genre("folk-punk"), "Folk-Punk");
        assert_eq!(normalize_genre("hip-hop"), "Hip-Hop");
        assert_eq!(normalize_genre("lo-fi"), "Lo-Fi");
        // Slash + hyphen combos still work.
        assert_eq!(normalize_genre("Folk-Punk/Ska-Punk"), "Folk-Punk/Ska-Punk");
        // Single-word genres are unaffected.
        assert_eq!(normalize_genre("rock"), "Rock");
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
    fn try_read_metadata_dsf_title_not_filename() {
        // Regression (LANDES Philippe / Benjithom): a tagged DSF must surface its
        // real ID3v2 title through the full try_read_metadata path, never fall
        // back to the filename. Covers the case where lofty parses the container
        // and returns a (possibly title-less) tag: our DSF ID3v2 parser must
        // still fill the title.
        use std::io::Write;
        let id3_tag = build_id3v2_tag(&[("TIT2", "Aurora"), ("TPE1", "Yes"), ("TALB", "Fragile")]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        let tmp = std::env::temp_dir().join("tune_test_dsf_title_e2e.dsf");
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = try_read_metadata(&tmp);
        std::fs::remove_file(&tmp).ok();
        let meta = meta.expect("try_read_metadata should succeed for a tagged DSF");
        assert_eq!(meta.title.as_deref(), Some("Aurora"));
        assert_eq!(meta.artist.as_deref(), Some("Yes"));
        assert_eq!(meta.album.as_deref(), Some("Fragile"));
        assert_eq!(meta.format.as_deref(), Some("dsd"));
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
            cover_art: None,
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
    fn parse_id3v22_maps_three_char_frames() {
        // ID3v2.2 tag (3-char frame ids, 6-byte frame header) as used by many
        // DSD/DSF files — previously skipped, so the title fell back to filename.
        let build_frame = |id: &[u8; 3], text: &str| {
            let body_len = 1 + text.len(); // encoding byte + text
            let mut f = Vec::new();
            f.extend_from_slice(id);
            f.extend_from_slice(&[
                (body_len >> 16) as u8,
                (body_len >> 8) as u8,
                body_len as u8,
            ]);
            f.push(0x00); // ISO-8859-1
            f.extend_from_slice(text.as_bytes());
            f
        };
        let mut frames = Vec::new();
        frames.extend(build_frame(b"TT2", "The Beat Goes On"));
        frames.extend(build_frame(b"TP1", "Sonny & Cher"));
        frames.extend(build_frame(b"TAL", "Best Of"));

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[0x02, 0x00, 0x00]); // v2.2.0, no flags
        let size = frames.len();
        tag.extend_from_slice(&[
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        tag.extend_from_slice(&frames);

        let tags = parse_id3v2_tag(&tag).unwrap();
        assert_eq!(tags.title(), Some("The Beat Goes On"));
        assert_eq!(tags.artist(), Some("Sonny & Cher"));
        assert_eq!(tags.album(), Some("Best Of"));
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
