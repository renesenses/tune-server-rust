//! DFF (DSDIFF) file parser.
//!
//! DSDIFF format specification (IFF-based, big-endian):
//! - FRM8 chunk: "FRM8" magic, chunk size (u64 BE), "DSD " form type
//! - Property chunk "PROP": contains sub-chunks:
//!   - "FS  ": sample rate (u32 BE)
//!   - "CHNL": channel count (u16 BE) + channel IDs
//!   - "CMPR": compression type (4 bytes: "DSD " or "DST ")
//! - DSD Sound Data chunk "DSD ": raw DSD data (interleaved by sample)
//!
//! All multi-byte values are big-endian.
//! DSD bit ordering: MSB first within each byte.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Parsed DFF (DSDIFF) file header information.
#[derive(Debug, Clone)]
pub struct DffInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub compression: String,
    pub data_offset: u64,
    pub data_size: u64,
}

/// Read a big-endian u16 from a byte slice at the given offset.
fn read_u16_be(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

/// Read a big-endian u32 from a byte slice at the given offset.
fn read_u32_be(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Read a big-endian u64 from a byte slice at the given offset.
fn read_u64_be(buf: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

/// Parse a DFF (DSDIFF) file and return metadata needed for decoding.
pub fn parse_dff(path: &str) -> Result<DffInfo, String> {
    let mut file = File::open(path).map_err(|e| format!("dff open: {e}"))?;

    // --- FRM8 header (12 bytes): "FRM8" + size(u64) + "DSD " ---
    let mut frm8 = [0u8; 12];
    file.read_exact(&mut frm8)
        .map_err(|e| format!("dff read FRM8: {e}"))?;

    if &frm8[0..4] != b"FRM8" {
        return Err("not a DFF file: missing 'FRM8' magic".into());
    }

    // FRM8 chunk size (not including the 12-byte header)
    let _frm8_size = read_u64_be(&frm8, 4);

    // Read form type
    let mut form_type = [0u8; 4];
    file.read_exact(&mut form_type)
        .map_err(|e| format!("dff read form type: {e}"))?;

    if &form_type != b"DSD " {
        return Err(format!(
            "not a DFF/DSD file: form type is '{}'",
            String::from_utf8_lossy(&form_type)
        ));
    }

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u32> = None;
    let mut compression: Option<String> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_size: Option<u64> = None;

    // Parse chunks until we have all the info we need
    loop {
        let pos = file
            .stream_position()
            .map_err(|e| format!("dff stream_position: {e}"))?;

        // Read chunk header: 4 bytes ID + 8 bytes size
        let mut chunk_header = [0u8; 12];
        if file.read_exact(&mut chunk_header).is_err() {
            break; // EOF
        }

        let chunk_id = &chunk_header[0..4];
        let chunk_size = read_u64_be(&chunk_header, 4);

        match chunk_id {
            b"PROP" => {
                // Property chunk: read the "SND " sub-type, then parse sub-chunks
                let mut prop_type = [0u8; 4];
                file.read_exact(&mut prop_type)
                    .map_err(|e| format!("dff read PROP type: {e}"))?;

                if &prop_type != b"SND " {
                    // Skip non-SND property chunks
                    let skip = chunk_size.saturating_sub(4);
                    file.seek(SeekFrom::Current(skip as i64))
                        .map_err(|e| format!("dff skip PROP: {e}"))?;
                    continue;
                }

                // Parse sub-chunks within PROP/SND
                let prop_end = pos + 12 + chunk_size;
                while file
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < prop_end
                {
                    let mut sub_header = [0u8; 12];
                    if file.read_exact(&mut sub_header).is_err() {
                        break;
                    }

                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    match &sub_id {
                        b"FS  " => {
                            let mut fs_buf = [0u8; 4];
                            file.read_exact(&mut fs_buf)
                                .map_err(|e| format!("dff read FS: {e}"))?;
                            sample_rate = Some(read_u32_be(&fs_buf, 0));
                            // Skip any remaining bytes in this sub-chunk
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip FS extra: {e}"))?;
                            }
                        }
                        b"CHNL" => {
                            let mut chnl_buf = [0u8; 2];
                            file.read_exact(&mut chnl_buf)
                                .map_err(|e| format!("dff read CHNL: {e}"))?;
                            channels = Some(read_u16_be(&chnl_buf, 0) as u32);
                            // Skip channel ID bytes
                            let skip = sub_size.saturating_sub(2);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip CHNL ids: {e}"))?;
                            }
                        }
                        b"CMPR" => {
                            let mut cmpr_buf = [0u8; 4];
                            file.read_exact(&mut cmpr_buf)
                                .map_err(|e| format!("dff read CMPR: {e}"))?;
                            compression = Some(String::from_utf8_lossy(&cmpr_buf).to_string());
                            // Skip any remaining bytes (e.g. compression name string)
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip CMPR extra: {e}"))?;
                            }
                        }
                        _ => {
                            // Skip unknown sub-chunk
                            // Pad to even boundary (IFF rule)
                            let padded = (sub_size + 1) & !1;
                            file.seek(SeekFrom::Current(padded as i64))
                                .map_err(|e| format!("dff skip sub-chunk: {e}"))?;
                        }
                    }
                }
            }
            b"DSD " => {
                // DSD Sound Data chunk — the actual audio samples
                data_offset = Some(pos + 12); // data starts right after the chunk header
                data_size = Some(chunk_size);
                // Don't need to read past this for header parsing
                break;
            }
            _ => {
                // Skip unknown chunk (pad to even boundary per IFF spec)
                let padded = (chunk_size + 1) & !1;
                file.seek(SeekFrom::Current(padded as i64))
                    .map_err(|e| format!("dff skip chunk: {e}"))?;
            }
        }
    }

    let sample_rate = sample_rate.ok_or("DFF: missing FS (sample rate) sub-chunk")?;
    let channels = channels.ok_or("DFF: missing CHNL (channels) sub-chunk")?;
    let compression = compression.unwrap_or_else(|| "DSD ".into());
    let data_offset = data_offset.ok_or("DFF: missing DSD sound data chunk")?;
    let data_size = data_size.ok_or("DFF: missing DSD sound data chunk size")?;

    if channels == 0 || channels > 8 {
        return Err(format!("invalid channel count: {channels}"));
    }
    if sample_rate < 2_000_000 || sample_rate > 50_000_000 {
        return Err(format!("unexpected DSD sample rate: {sample_rate}"));
    }

    Ok(DffInfo {
        channels,
        sample_rate,
        compression,
        data_offset,
        data_size,
    })
}

/// Read all DSD sample data from a DFF file.
///
/// DFF stores data interleaved by sample (MSB first per byte):
/// byte layout is already ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...
/// so no de-interleaving is needed — just read the raw bytes.
pub fn read_dff_data(path: &str, info: &DffInfo) -> Result<Vec<u8>, String> {
    if info.compression != "DSD " {
        return Err(format!(
            "DFF: unsupported compression '{}' (only uncompressed DSD supported)",
            info.compression
        ));
    }

    let mut file = File::open(path).map_err(|e| format!("dff open: {e}"))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|e| format!("dff seek: {e}"))?;

    let mut data = vec![0u8; info.data_size as usize];
    file.read_exact(&mut data)
        .map_err(|e| format!("dff read data: {e}"))?;

    Ok(data)
}

/// Parse DFF header from an in-memory buffer (for testing).
pub fn parse_dff_from_bytes(data: &[u8]) -> Result<DffInfo, String> {
    use std::io::Cursor;

    if data.len() < 16 {
        return Err("buffer too small for DFF header".into());
    }

    // Use a cursor to simulate file I/O
    let mut cursor = Cursor::new(data);

    // FRM8 header
    let mut frm8 = [0u8; 12];
    cursor
        .read_exact(&mut frm8)
        .map_err(|e| format!("dff read FRM8: {e}"))?;

    if &frm8[0..4] != b"FRM8" {
        return Err("not a DFF file: missing 'FRM8' magic".into());
    }

    let mut form_type = [0u8; 4];
    cursor
        .read_exact(&mut form_type)
        .map_err(|e| format!("dff read form type: {e}"))?;

    if &form_type != b"DSD " {
        return Err("not a DFF/DSD file".into());
    }

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u32> = None;
    let mut compression: Option<String> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_size: Option<u64> = None;

    loop {
        let pos = cursor
            .stream_position()
            .map_err(|e| format!("dff pos: {e}"))?;

        let mut chunk_header = [0u8; 12];
        if cursor.read_exact(&mut chunk_header).is_err() {
            break;
        }

        let chunk_id = [
            chunk_header[0],
            chunk_header[1],
            chunk_header[2],
            chunk_header[3],
        ];
        let chunk_size = read_u64_be(&chunk_header, 4);

        match &chunk_id {
            b"PROP" => {
                let mut prop_type = [0u8; 4];
                cursor
                    .read_exact(&mut prop_type)
                    .map_err(|e| format!("dff read PROP type: {e}"))?;

                if &prop_type != b"SND " {
                    let skip = chunk_size.saturating_sub(4);
                    cursor
                        .seek(SeekFrom::Current(skip as i64))
                        .map_err(|e| format!("dff skip: {e}"))?;
                    continue;
                }

                let prop_end = pos + 12 + chunk_size;
                while cursor
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < prop_end
                {
                    let mut sub_header = [0u8; 12];
                    if cursor.read_exact(&mut sub_header).is_err() {
                        break;
                    }

                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    match &sub_id {
                        b"FS  " => {
                            let mut fs_buf = [0u8; 4];
                            cursor
                                .read_exact(&mut fs_buf)
                                .map_err(|e| format!("dff read FS: {e}"))?;
                            sample_rate = Some(read_u32_be(&fs_buf, 0));
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        b"CHNL" => {
                            let mut chnl_buf = [0u8; 2];
                            cursor
                                .read_exact(&mut chnl_buf)
                                .map_err(|e| format!("dff read CHNL: {e}"))?;
                            channels = Some(read_u16_be(&chnl_buf, 0) as u32);
                            let skip = sub_size.saturating_sub(2);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        b"CMPR" => {
                            let mut cmpr_buf = [0u8; 4];
                            cursor
                                .read_exact(&mut cmpr_buf)
                                .map_err(|e| format!("dff read CMPR: {e}"))?;
                            compression = Some(String::from_utf8_lossy(&cmpr_buf).to_string());
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        _ => {
                            let padded = (sub_size + 1) & !1;
                            cursor
                                .seek(SeekFrom::Current(padded as i64))
                                .map_err(|e| format!("dff skip: {e}"))?;
                        }
                    }
                }
            }
            b"DSD " => {
                data_offset = Some(pos + 12);
                data_size = Some(chunk_size);
                break;
            }
            _ => {
                let padded = (chunk_size + 1) & !1;
                cursor
                    .seek(SeekFrom::Current(padded as i64))
                    .map_err(|e| format!("dff skip: {e}"))?;
            }
        }
    }

    let sample_rate = sample_rate.ok_or("DFF: missing FS sub-chunk")?;
    let channels = channels.ok_or("DFF: missing CHNL sub-chunk")?;
    let compression = compression.unwrap_or_else(|| "DSD ".into());
    let data_offset = data_offset.ok_or("DFF: missing DSD data chunk")?;
    let data_size = data_size.ok_or("DFF: missing DSD data chunk")?;

    Ok(DffInfo {
        channels,
        sample_rate,
        compression,
        data_offset,
        data_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid DFF header in memory.
    fn build_dff_header(channels: u16, sample_rate: u32, dsd_data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        // --- Build PROP chunk content first to know its size ---
        let mut prop_content = Vec::new();
        prop_content.extend_from_slice(b"SND "); // 4 bytes

        // FS sub-chunk: "FS  " + size(8) + sample_rate(4)
        prop_content.extend_from_slice(b"FS  ");
        prop_content.extend_from_slice(&4u64.to_be_bytes());
        prop_content.extend_from_slice(&sample_rate.to_be_bytes());

        // CHNL sub-chunk: "CHNL" + size(8) + channel_count(2)
        prop_content.extend_from_slice(b"CHNL");
        let chnl_data_size = 2u64; // just the count, no channel IDs for simplicity
        prop_content.extend_from_slice(&chnl_data_size.to_be_bytes());
        prop_content.extend_from_slice(&channels.to_be_bytes());

        // CMPR sub-chunk: "CMPR" + size(8) + "DSD "(4)
        prop_content.extend_from_slice(b"CMPR");
        prop_content.extend_from_slice(&4u64.to_be_bytes());
        prop_content.extend_from_slice(b"DSD ");

        let prop_chunk_size = prop_content.len() as u64;

        // DSD data chunk size
        let dsd_chunk_size = dsd_data.len() as u64;

        // Total FRM8 content size: 4 (form type "DSD ") + 12 (PROP header) + prop_content
        //                          + 12 (DSD header) + dsd_data
        let frm8_content_size = 4 + 12 + prop_chunk_size + 12 + dsd_chunk_size;

        // --- FRM8 header ---
        buf.extend_from_slice(b"FRM8");
        buf.extend_from_slice(&frm8_content_size.to_be_bytes());
        buf.extend_from_slice(b"DSD "); // form type

        // --- PROP chunk ---
        buf.extend_from_slice(b"PROP");
        buf.extend_from_slice(&prop_chunk_size.to_be_bytes());
        buf.extend_from_slice(&prop_content);

        // --- DSD Sound Data chunk ---
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&dsd_chunk_size.to_be_bytes());
        buf.extend_from_slice(dsd_data);

        buf
    }

    #[test]
    fn parse_valid_dff_header() {
        let dsd_data = vec![0u8; 4096];
        let buf = build_dff_header(2, 2_822_400, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 2_822_400);
        assert_eq!(info.compression, "DSD ");
        assert_eq!(info.data_size, 4096);
    }

    #[test]
    fn parse_dff_dsd128() {
        let dsd_data = vec![0u8; 8192];
        let buf = build_dff_header(2, 5_644_800, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.sample_rate, 5_644_800);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn parse_dff_mono() {
        let dsd_data = vec![0u8; 2048];
        let buf = build_dff_header(1, 2_822_400, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn parse_dff_bad_magic() {
        let mut buf = build_dff_header(2, 2_822_400, &[0u8; 1024]);
        buf[0] = b'X'; // corrupt "FRM8"
        let result = parse_dff_from_bytes(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("FRM8"));
    }

    #[test]
    fn parse_dff_bad_form_type() {
        let mut buf = build_dff_header(2, 2_822_400, &[0u8; 1024]);
        buf[12] = b'X'; // corrupt "DSD " form type
        let result = parse_dff_from_bytes(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn parse_dff_too_short() {
        let result = parse_dff_from_bytes(&[0u8; 10]);
        assert!(result.is_err());
    }
}
