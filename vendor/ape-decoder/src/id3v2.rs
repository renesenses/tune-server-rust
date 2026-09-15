//! ID3v2.3 and ID3v2.4 tag reader for APE files.
//!
//! APE files may contain an ID3v2 tag prepended before the APE header (in the
//! "junk header" region). This module parses those tags and exposes the most
//! common text frames.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{ApeError, ApeResult};

/// Maximum tag size we are willing to allocate (16 MiB).
const MAX_TAG_SIZE: u32 = 16 * 1024 * 1024;

/// A single ID3v2 frame.
#[derive(Debug, Clone)]
pub struct Id3v2Frame {
    /// Four-character frame identifier (e.g. "TIT2").
    pub id: String,
    /// Raw frame payload (excluding the 10-byte frame header).
    pub data: Vec<u8>,
}

/// A parsed ID3v2 tag.
#[derive(Debug, Clone)]
pub struct Id3v2Tag {
    /// ID3v2 version as `(major, revision)`, e.g. `(3, 0)` or `(4, 0)`.
    pub version: (u8, u8),
    /// Parsed frames.
    pub frames: Vec<Id3v2Frame>,
}

impl Id3v2Tag {
    /// Look up a text frame by its four-character ID and decode it to a string.
    fn text_frame(&self, id: &str) -> Option<String> {
        self.frames
            .iter()
            .find(|f| f.id == id)
            .and_then(|f| decode_text_frame(&f.data))
    }

    /// Title (TIT2).
    pub fn title(&self) -> Option<String> {
        self.text_frame("TIT2")
    }

    /// Artist (TPE1).
    pub fn artist(&self) -> Option<String> {
        self.text_frame("TPE1")
    }

    /// Album (TALB).
    pub fn album(&self) -> Option<String> {
        self.text_frame("TALB")
    }

    /// Year -- TYER for v2.3, TDRC for v2.4.
    pub fn year(&self) -> Option<String> {
        self.text_frame("TDRC").or_else(|| self.text_frame("TYER"))
    }

    /// Track number (TRCK).
    pub fn track(&self) -> Option<String> {
        self.text_frame("TRCK")
    }

    /// Genre (TCON).
    pub fn genre(&self) -> Option<String> {
        self.text_frame("TCON")
    }

    /// Comment -- not a text frame (COMM), but we attempt a simple extraction.
    pub fn comment(&self) -> Option<String> {
        self.frames
            .iter()
            .find(|f| f.id == "COMM")
            .and_then(|f| decode_comment_frame(&f.data))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read an ID3v2 tag from the beginning of the stream.
///
/// Returns `Ok(None)` if the stream does not start with an ID3v2 header.
pub fn read_id3v2<R: Read + Seek>(reader: &mut R) -> ApeResult<Option<Id3v2Tag>> {
    reader.seek(SeekFrom::Start(0))?;

    // -- Read 10-byte header --
    let mut header = [0u8; 10];
    if reader.read(&mut header)? < 10 {
        return Ok(None);
    }

    // Magic bytes "ID3"
    if &header[0..3] != b"ID3" {
        return Ok(None);
    }

    let major = header[3];
    let revision = header[4];
    let flags = header[5];

    // We support v2.3 and v2.4 only.
    if major != 3 && major != 4 {
        return Err(ApeError::InvalidFormat("unsupported ID3v2 version"));
    }

    // Reject unsynchronization (bit 7).
    if flags & 0x80 != 0 {
        return Err(ApeError::InvalidFormat(
            "ID3v2 unsynchronization is not supported",
        ));
    }

    let size = decode_syncsafe(&header[6..10]);
    if size > MAX_TAG_SIZE {
        return Err(ApeError::InvalidFormat("ID3v2 tag too large"));
    }

    // -- Read tag body --
    let mut tag_data = vec![0u8; size as usize];
    let bytes_read = read_full(reader, &mut tag_data)?;
    tag_data.truncate(bytes_read);

    // If extended header flag (bit 6) is set, skip it.
    let mut offset = 0usize;
    if flags & 0x40 != 0 {
        if tag_data.len() < 4 {
            return Ok(Some(Id3v2Tag {
                version: (major, revision),
                frames: Vec::new(),
            }));
        }
        let ext_size = if major == 4 {
            decode_syncsafe(&tag_data[0..4]) as usize
        } else {
            u32::from_be_bytes([tag_data[0], tag_data[1], tag_data[2], tag_data[3]]) as usize
        };
        // v2.3: ext_size excludes its own 4 bytes; v2.4: includes them.
        offset = if major == 4 { ext_size } else { ext_size + 4 };
        if offset > tag_data.len() {
            offset = tag_data.len();
        }
    }

    // -- Parse frames --
    let frames = parse_frames(&tag_data[offset..], major)?;

    Ok(Some(Id3v2Tag {
        version: (major, revision),
        frames,
    }))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Decode a 4-byte sync-safe integer (each byte uses only 7 bits).
fn decode_syncsafe(b: &[u8]) -> u32 {
    ((b[0] as u32) << 21) | ((b[1] as u32) << 14) | ((b[2] as u32) << 7) | (b[3] as u32)
}

/// Read as many bytes as possible (may be fewer than `buf.len()` at EOF).
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> ApeResult<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// Parse ID3v2 frames from tag body bytes.
fn parse_frames(data: &[u8], major: u8) -> ApeResult<Vec<Id3v2Frame>> {
    let mut frames = Vec::new();
    let mut pos = 0;

    loop {
        // Need at least 10 bytes for a frame header.
        if pos + 10 > data.len() {
            break;
        }

        // Frame ID is 4 bytes -- if the first byte is 0x00 we've hit padding.
        if data[pos] == 0x00 {
            break;
        }

        let id_bytes = &data[pos..pos + 4];
        // Validate frame ID: each byte should be A-Z or 0-9.
        if !id_bytes.iter().all(|&b| b.is_ascii_alphanumeric()) {
            break;
        }

        let id = String::from_utf8_lossy(id_bytes).into_owned();

        let frame_size = if major == 4 {
            decode_syncsafe(&data[pos + 4..pos + 8])
        } else {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
        } as usize;

        // Skip flags (2 bytes at pos+8..pos+10).
        pos += 10;

        if frame_size == 0 || pos + frame_size > data.len() {
            // Truncated or zero-length frame -- stop parsing.
            break;
        }

        let frame_data = data[pos..pos + frame_size].to_vec();
        pos += frame_size;

        frames.push(Id3v2Frame {
            id,
            data: frame_data,
        });
    }

    Ok(frames)
}

/// Decode the text content of a standard text frame (IDs starting with 'T',
/// excluding "TXXX").
fn decode_text_frame(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }

    let encoding = data[0];
    let payload = &data[1..];

    if payload.is_empty() {
        return None;
    }

    let text = match encoding {
        0 => decode_iso_8859_1(payload),
        1 => decode_utf16_with_bom(payload),
        2 => decode_utf16be(payload),
        3 => decode_utf8(payload),
        _ => return None,
    };

    // Trim trailing NULs that some encoders include.
    let text = text.trim_end_matches('\0').to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Decode a COMM (comment) frame.  Layout:
///   encoding(1) + language(3) + short-description(NUL-terminated) + text
fn decode_comment_frame(data: &[u8]) -> Option<String> {
    if data.len() < 5 {
        return None;
    }

    let encoding = data[0];
    // Skip language (3 bytes).
    let rest = &data[4..];

    // Find the NUL terminator(s) separating the short description from the
    // actual comment text.  For encoding 0/3 the terminator is a single 0x00;
    // for 1/2 it is 0x00 0x00.
    let (text_start, _) = match encoding {
        0 | 3 => {
            let nul = rest.iter().position(|&b| b == 0)?;
            (nul + 1, &rest[..nul])
        }
        1 | 2 => {
            let nul = find_double_nul(rest)?;
            (nul + 2, &rest[..nul])
        }
        _ => return None,
    };

    if text_start >= rest.len() {
        return None;
    }

    let payload = &rest[text_start..];
    let text = match encoding {
        0 => decode_iso_8859_1(payload),
        1 => decode_utf16_with_bom(payload),
        2 => decode_utf16be(payload),
        3 => decode_utf8(payload),
        _ => return None,
    };

    let text = text.trim_end_matches('\0').to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Find the position of a double-NUL (0x00 0x00) on an even byte boundary.
fn find_double_nul(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            return Some(i);
        }
        i += 2;
    }
    None
}

// -- Text encoding helpers --

fn decode_iso_8859_1(data: &[u8]) -> String {
    data.iter().map(|&b| b as char).collect()
}

fn decode_utf8(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

fn decode_utf16_with_bom(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }

    let (big_endian, payload) = if data[0] == 0xFE && data[1] == 0xFF {
        (true, &data[2..])
    } else if data[0] == 0xFF && data[1] == 0xFE {
        (false, &data[2..])
    } else {
        // No BOM -- assume little-endian (common in practice).
        (false, data)
    };

    decode_utf16_raw(payload, big_endian)
}

fn decode_utf16be(data: &[u8]) -> String {
    decode_utf16_raw(data, true)
}

fn decode_utf16_raw(data: &[u8], big_endian: bool) -> String {
    let code_units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| {
            if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            }
        })
        .collect();

    String::from_utf16_lossy(&code_units)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a sync-safe 4-byte encoding of `value`.
    fn encode_syncsafe(value: u32) -> [u8; 4] {
        [
            ((value >> 21) & 0x7F) as u8,
            ((value >> 14) & 0x7F) as u8,
            ((value >> 7) & 0x7F) as u8,
            (value & 0x7F) as u8,
        ]
    }

    /// Helper: build a minimal ID3v2 tag byte vector.
    fn build_id3v2_tag(major: u8, flags: u8, frames_data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ID3");
        buf.push(major); // version major
        buf.push(0); // version revision
        buf.push(flags);
        let size = encode_syncsafe(frames_data.len() as u32);
        buf.extend_from_slice(&size);
        buf.extend_from_slice(frames_data);
        buf
    }

    /// Build a v2.3 text frame: ID(4) + size(4 BE u32) + flags(2) + data.
    fn build_v23_text_frame(id: &str, encoding: u8, text: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(id.as_bytes());
        let data_len = 1 + text.len(); // encoding byte + text
        frame.extend_from_slice(&(data_len as u32).to_be_bytes());
        frame.extend_from_slice(&[0x00, 0x00]); // flags
        frame.push(encoding);
        frame.extend_from_slice(text);
        frame
    }

    /// Build a v2.4 text frame: ID(4) + size(4 sync-safe) + flags(2) + data.
    fn build_v24_text_frame(id: &str, encoding: u8, text: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(id.as_bytes());
        let data_len = 1 + text.len();
        frame.extend_from_slice(&encode_syncsafe(data_len as u32));
        frame.extend_from_slice(&[0x00, 0x00]); // flags
        frame.push(encoding);
        frame.extend_from_slice(text);
        frame
    }

    // --- v2.3 tests ---

    #[test]
    fn test_parse_id3v23_iso8859() {
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 0, b"Hello World"));
        frames_data.extend_from_slice(&build_v23_text_frame("TPE1", 0, b"Test Artist"));
        frames_data.extend_from_slice(&build_v23_text_frame("TALB", 0, b"Test Album"));
        frames_data.extend_from_slice(&build_v23_text_frame("TYER", 0, b"2024"));
        frames_data.extend_from_slice(&build_v23_text_frame("TRCK", 0, b"7"));
        frames_data.extend_from_slice(&build_v23_text_frame("TCON", 0, b"Rock"));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.version, (3, 0));
        assert_eq!(tag.frames.len(), 6);
        assert_eq!(tag.title().as_deref(), Some("Hello World"));
        assert_eq!(tag.artist().as_deref(), Some("Test Artist"));
        assert_eq!(tag.album().as_deref(), Some("Test Album"));
        assert_eq!(tag.year().as_deref(), Some("2024"));
        assert_eq!(tag.track().as_deref(), Some("7"));
        assert_eq!(tag.genre().as_deref(), Some("Rock"));
    }

    #[test]
    fn test_parse_id3v23_utf8() {
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame(
            "TIT2",
            3,
            "Caf\u{00e9} Music".as_bytes(),
        ));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Caf\u{00e9} Music"));
    }

    #[test]
    fn test_parse_id3v23_utf16_bom_le() {
        // UTF-16 LE with BOM: FF FE
        let text_utf16: Vec<u8> = {
            let mut v = vec![0xFF, 0xFE]; // BOM LE
            for ch in "Hello".encode_utf16() {
                v.extend_from_slice(&ch.to_le_bytes());
            }
            v
        };

        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 1, &text_utf16));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Hello"));
    }

    #[test]
    fn test_parse_id3v23_utf16_bom_be() {
        // UTF-16 BE with BOM: FE FF
        let text_utf16: Vec<u8> = {
            let mut v = vec![0xFE, 0xFF]; // BOM BE
            for ch in "World".encode_utf16() {
                v.extend_from_slice(&ch.to_be_bytes());
            }
            v
        };

        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 1, &text_utf16));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.title().as_deref(), Some("World"));
    }

    // --- v2.4 tests ---

    #[test]
    fn test_parse_id3v24_syncsafe_sizes() {
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v24_text_frame("TIT2", 3, b"v2.4 Title"));
        frames_data.extend_from_slice(&build_v24_text_frame("TPE1", 3, b"v2.4 Artist"));
        frames_data.extend_from_slice(&build_v24_text_frame("TDRC", 3, b"2025"));

        let tag_bytes = build_id3v2_tag(4, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.version, (4, 0));
        assert_eq!(tag.title().as_deref(), Some("v2.4 Title"));
        assert_eq!(tag.artist().as_deref(), Some("v2.4 Artist"));
        assert_eq!(tag.year().as_deref(), Some("2025"));
    }

    #[test]
    fn test_parse_id3v24_year_falls_back_to_tyer() {
        // A v2.4 tag that only has TYER (unusual but should work).
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v24_text_frame("TYER", 3, b"1999"));

        let tag_bytes = build_id3v2_tag(4, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.year().as_deref(), Some("1999"));
    }

    // --- Edge cases ---

    #[test]
    fn test_no_id3_header_returns_none() {
        let data = b"MAC \x00\x00\x00\x00some APE data";
        let mut cursor = Cursor::new(data.to_vec());
        let result = read_id3v2(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_stream_returns_none() {
        let mut cursor = Cursor::new(Vec::new());
        let result = read_id3v2(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_truncated_header_returns_none() {
        let mut cursor = Cursor::new(b"ID3".to_vec());
        let result = read_id3v2(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_zero_length_text_frame() {
        // A frame with encoding byte but no text.
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 0, b""));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert!(tag.title().is_none());
    }

    #[test]
    fn test_invalid_encoding_byte() {
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 99, b"Bad Encoding"));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert!(tag.title().is_none());
    }

    #[test]
    fn test_unsynchronization_rejected() {
        let frames_data = build_v23_text_frame("TIT2", 0, b"Test");
        let tag_bytes = build_id3v2_tag(3, 0x80, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let result = read_id3v2(&mut cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_iso8859_high_bytes() {
        // ISO-8859-1 with characters above 0x7F.
        let text: Vec<u8> = vec![0xC9, 0x6C, 0xE8, 0x76, 0x65]; // "Eleve" with accents
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 0, &text));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        let title = tag.title().unwrap();
        assert_eq!(title, "\u{00C9}l\u{00E8}ve");
    }

    #[test]
    fn test_text_with_trailing_nul() {
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 0, b"Trimmed\x00"));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Trimmed"));
    }

    #[test]
    fn test_multiple_frames_mixed_encodings() {
        let utf16_text: Vec<u8> = {
            let mut v = vec![0xFF, 0xFE]; // BOM LE
            for ch in "UTF-16 Title".encode_utf16() {
                v.extend_from_slice(&ch.to_le_bytes());
            }
            v
        };

        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 1, &utf16_text));
        frames_data.extend_from_slice(&build_v23_text_frame("TPE1", 0, b"Latin1 Artist"));
        frames_data.extend_from_slice(&build_v23_text_frame("TALB", 3, "UTF-8 Album".as_bytes()));

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.title().as_deref(), Some("UTF-16 Title"));
        assert_eq!(tag.artist().as_deref(), Some("Latin1 Artist"));
        assert_eq!(tag.album().as_deref(), Some("UTF-8 Album"));
    }

    #[test]
    fn test_padding_after_frames() {
        // Frames followed by zero padding.
        let mut frames_data = Vec::new();
        frames_data.extend_from_slice(&build_v23_text_frame("TIT2", 0, b"Padded"));
        frames_data.extend_from_slice(&[0u8; 64]); // padding

        let tag_bytes = build_id3v2_tag(3, 0, &frames_data);
        let mut cursor = Cursor::new(tag_bytes);

        let tag = read_id3v2(&mut cursor).unwrap().unwrap();
        assert_eq!(tag.frames.len(), 1);
        assert_eq!(tag.title().as_deref(), Some("Padded"));
    }

    #[test]
    fn test_real_ape_fixtures_no_crash() {
        use std::fs::File;
        use std::io::BufReader;
        use std::path::PathBuf;

        let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ape");

        let entries = std::fs::read_dir(&fixtures_dir);
        if entries.is_err() {
            // Fixtures not available -- skip.
            return;
        }

        for entry in entries.unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ape") {
                let file = File::open(&path).unwrap();
                let mut reader = BufReader::new(file);
                // Should not panic regardless of content.
                let result = read_id3v2(&mut reader);
                // Most test APE files won't have ID3v2 tags.
                match result {
                    Ok(None) => {}    // expected
                    Ok(Some(_)) => {} // also fine
                    Err(_) => {}      // acceptable for malformed data
                }
            }
        }
    }
}
