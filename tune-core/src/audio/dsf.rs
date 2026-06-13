//! DSF (DSD Stream File) parser.
//!
//! DSF format specification:
//! - DSD Chunk (28 bytes): magic "DSD ", chunk size, file size, metadata offset
//! - Format Chunk (52 bytes): magic "fmt ", chunk size, format version, format ID,
//!   channel type, channel count, sample rate, bits per sample, sample count, block size
//! - Data Chunk: magic "data", chunk size, interleaved DSD blocks
//! - Metadata Chunk (optional): ID3v2 tags at metadata offset
//!
//! All multi-byte values are little-endian.
//! DSD bit ordering: LSB first within each byte.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Parsed DSF file header information.
#[derive(Debug, Clone)]
pub struct DsfInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    pub block_size: u32,
    pub data_offset: u64,
    pub data_size: u64,
}

/// Read a little-endian u32 from a byte slice at the given offset.
fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Read a little-endian u64 from a byte slice at the given offset.
fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
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

/// Parse a DSF file header and return metadata needed for decoding.
pub fn parse_dsf(path: &str) -> Result<DsfInfo, String> {
    let mut file = File::open(path).map_err(|e| format!("dsf open: {e}"))?;

    // --- DSD Chunk (28 bytes) ---
    let mut dsd_chunk = [0u8; 28];
    file.read_exact(&mut dsd_chunk)
        .map_err(|e| format!("dsf read DSD chunk: {e}"))?;

    if &dsd_chunk[0..4] != b"DSD " {
        return Err("not a DSF file: missing 'DSD ' magic".into());
    }

    let dsd_chunk_size = read_u64_le(&dsd_chunk, 4);
    if dsd_chunk_size != 28 {
        return Err(format!(
            "unexpected DSD chunk size: {dsd_chunk_size} (expected 28)"
        ));
    }

    // total file size and metadata offset are informational
    // let _total_file_size = read_u64_le(&dsd_chunk, 12);
    // let _metadata_offset = read_u64_le(&dsd_chunk, 20);

    // --- Format Chunk (52 bytes) ---
    let mut fmt_chunk = [0u8; 52];
    file.read_exact(&mut fmt_chunk)
        .map_err(|e| format!("dsf read fmt chunk: {e}"))?;

    if &fmt_chunk[0..4] != b"fmt " {
        return Err("DSF: missing 'fmt ' chunk".into());
    }

    let fmt_chunk_size = read_u64_le(&fmt_chunk, 4);
    if fmt_chunk_size != 52 {
        return Err(format!(
            "unexpected fmt chunk size: {fmt_chunk_size} (expected 52)"
        ));
    }

    let _format_version = read_u32_le(&fmt_chunk, 12);
    let format_id = read_u32_le(&fmt_chunk, 16);
    if format_id != 0 {
        return Err(format!(
            "unsupported DSF format ID: {format_id} (only DSD raw = 0 supported)"
        ));
    }

    let _channel_type = read_u32_le(&fmt_chunk, 20);
    let channels = read_u32_le(&fmt_chunk, 24);
    let sample_rate = read_u32_le(&fmt_chunk, 28);
    let bits_per_sample = read_u32_le(&fmt_chunk, 32);
    let total_samples = read_u64_le(&fmt_chunk, 36);
    let block_size = read_u32_le(&fmt_chunk, 44);
    // reserved u32 at offset 48

    if channels == 0 || channels > 8 {
        return Err(format!("invalid channel count: {channels}"));
    }
    if sample_rate < 2_000_000 || sample_rate > 50_000_000 {
        return Err(format!("unexpected DSD sample rate: {sample_rate}"));
    }
    if block_size == 0 {
        return Err("DSF block size is zero".into());
    }

    // --- Data Chunk header (12 bytes: magic + size) ---
    let mut data_header = [0u8; 12];
    file.read_exact(&mut data_header)
        .map_err(|e| format!("dsf read data chunk header: {e}"))?;

    if &data_header[0..4] != b"data" {
        return Err("DSF: missing 'data' chunk".into());
    }

    let data_chunk_size = read_u64_le(&data_header, 4);
    // data_size = chunk size minus the 12-byte header
    let data_size = data_chunk_size.saturating_sub(12);

    // Current file position is the start of actual DSD sample data
    let data_offset = file
        .stream_position()
        .map_err(|e| format!("dsf stream_position: {e}"))?;

    Ok(DsfInfo {
        channels,
        sample_rate,
        bits_per_sample,
        total_samples,
        block_size,
        data_offset,
        data_size,
    })
}

/// Read all DSD sample blocks from a DSF file.
///
/// DSF stores data in interleaved blocks: for each "super-block",
/// there are `channels` consecutive blocks of `block_size` bytes.
/// This function reads the raw bytes and de-interleaves them into
/// a flat byte array with samples ordered: ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...
///
/// Each byte contains 8 DSD samples (LSB first in DSF format).
pub fn read_dsf_blocks(path: &str, info: &DsfInfo) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|e| format!("dsf open: {e}"))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|e| format!("dsf seek: {e}"))?;

    let block_size = info.block_size as usize;
    let channels = info.channels as usize;

    // Total bytes of actual DSD sample data per channel
    let total_bytes_per_channel = (info.total_samples + 7) / 8; // 8 samples per byte
    let total_bytes_per_channel = total_bytes_per_channel as usize;

    // Number of complete super-blocks (each contains one block per channel)
    let blocks_per_channel = (total_bytes_per_channel + block_size - 1) / block_size;

    // Output: interleaved by byte (not by block)
    // Layout: for each byte position b, we output ch0[b], ch1[b], ...
    let mut output = vec![0u8; total_bytes_per_channel * channels];

    // Buffer for one super-block (all channels)
    let super_block_size = block_size * channels;
    let mut super_block_buf = vec![0u8; super_block_size];

    let data_size = info.data_size as usize;
    let mut bytes_read_total = 0usize;

    for block_idx in 0..blocks_per_channel {
        let remaining_data = data_size.saturating_sub(bytes_read_total);
        let to_read = super_block_size.min(remaining_data);
        if to_read == 0 {
            break;
        }

        let buf = &mut super_block_buf[..to_read];
        file.read_exact(buf)
            .map_err(|e| format!("dsf read block {block_idx}: {e}"))?;
        bytes_read_total += to_read;

        // De-interleave: block layout is [ch0_block][ch1_block]...
        // We need to redistribute into byte-interleaved output
        let base_byte = block_idx * block_size;
        for ch in 0..channels {
            let ch_block_start = ch * block_size;
            let bytes_in_this_block =
                block_size.min(total_bytes_per_channel.saturating_sub(base_byte));
            let available = to_read.saturating_sub(ch_block_start);
            let copy_len = bytes_in_this_block.min(available);

            for b in 0..copy_len {
                let src_idx = ch_block_start + b;
                if src_idx >= to_read {
                    break;
                }
                let dst_idx = (base_byte + b) * channels + ch;
                if dst_idx < output.len() {
                    output[dst_idx] = super_block_buf[src_idx];
                }
            }
        }
    }

    Ok(output)
}

/// Streaming DSF reader that yields DSD data in byte-interleaved chunks.
///
/// Instead of loading the entire DSF data section into memory, this reads
/// one super-block at a time and de-interleaves it. Each `next_chunk()`
/// call returns one super-block's worth of byte-interleaved DSD data,
/// suitable for feeding to `DsdToPcmStreamer`.
///
/// Memory usage: O(block_size * channels) per call, typically ~8-32 KB.
pub struct DsfStreamReader {
    file: File,
    info: DsfInfo,
    block_idx: usize,
    blocks_per_channel: usize,
    bytes_read_total: usize,
    super_block_buf: Vec<u8>,
}

impl DsfStreamReader {
    /// Open a DSF file for streaming reading.
    pub fn open(path: &str, info: DsfInfo) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| format!("dsf open: {e}"))?;
        file.seek(SeekFrom::Start(info.data_offset))
            .map_err(|e| format!("dsf seek: {e}"))?;

        let block_size = info.block_size as usize;
        let channels = info.channels as usize;
        let total_bytes_per_channel = ((info.total_samples + 7) / 8) as usize;
        let blocks_per_channel = (total_bytes_per_channel + block_size - 1) / block_size;
        let super_block_size = block_size * channels;

        Ok(DsfStreamReader {
            file,
            info,
            block_idx: 0,
            blocks_per_channel,
            bytes_read_total: 0,
            super_block_buf: vec![0u8; super_block_size],
        })
    }

    /// Read the next chunk of byte-interleaved DSD data.
    ///
    /// Returns `Ok(Some(chunk))` with interleaved bytes, or `Ok(None)` at EOF.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.block_idx >= self.blocks_per_channel {
            return Ok(None);
        }

        let block_size = self.info.block_size as usize;
        let channels = self.info.channels as usize;
        let total_bytes_per_channel = ((self.info.total_samples + 7) / 8) as usize;
        let data_size = self.info.data_size as usize;
        let super_block_size = block_size * channels;

        let remaining_data = data_size.saturating_sub(self.bytes_read_total);
        let to_read = super_block_size.min(remaining_data);
        if to_read == 0 {
            return Ok(None);
        }

        let buf = &mut self.super_block_buf[..to_read];
        self.file
            .read_exact(buf)
            .map_err(|e| format!("dsf read block {}: {e}", self.block_idx))?;
        self.bytes_read_total += to_read;

        // De-interleave this super-block into byte-interleaved output
        let base_byte = self.block_idx * block_size;
        let bytes_in_this_block = block_size.min(total_bytes_per_channel.saturating_sub(base_byte));

        let mut output = vec![0u8; bytes_in_this_block * channels];

        for ch in 0..channels {
            let ch_block_start = ch * block_size;
            let available = to_read.saturating_sub(ch_block_start);
            let copy_len = bytes_in_this_block.min(available);

            for b in 0..copy_len {
                let src_idx = ch_block_start + b;
                if src_idx >= to_read {
                    break;
                }
                let dst_idx = b * channels + ch;
                if dst_idx < output.len() {
                    output[dst_idx] = self.super_block_buf[src_idx];
                }
            }
        }

        self.block_idx += 1;
        Ok(Some(output))
    }
}

/// Parse DSF header from an in-memory buffer (for testing).
pub fn parse_dsf_from_bytes(data: &[u8]) -> Result<DsfInfo, String> {
    if data.len() < 28 + 52 + 12 {
        return Err("buffer too small for DSF header".into());
    }

    // DSD Chunk
    if &data[0..4] != b"DSD " {
        return Err("not a DSF file: missing 'DSD ' magic".into());
    }
    let dsd_chunk_size = read_u64_le(data, 4);
    if dsd_chunk_size != 28 {
        return Err(format!("unexpected DSD chunk size: {dsd_chunk_size}"));
    }

    // Format Chunk (starts at byte 28)
    let fmt = &data[28..];
    if &fmt[0..4] != b"fmt " {
        return Err("DSF: missing 'fmt ' chunk".into());
    }
    let fmt_chunk_size = read_u64_le(fmt, 4);
    if fmt_chunk_size != 52 {
        return Err(format!("unexpected fmt chunk size: {fmt_chunk_size}"));
    }

    let format_id = read_u32_le(fmt, 16);
    if format_id != 0 {
        return Err(format!("unsupported DSF format ID: {format_id}"));
    }

    let channels = read_u32_le(fmt, 24);
    let sample_rate = read_u32_le(fmt, 28);
    let bits_per_sample = read_u32_le(fmt, 32);
    let total_samples = read_u64_le(fmt, 36);
    let block_size = read_u32_le(fmt, 44);

    // Data Chunk header (starts at byte 80)
    let data_hdr = &data[80..];
    if &data_hdr[0..4] != b"data" {
        return Err("DSF: missing 'data' chunk".into());
    }
    let data_chunk_size = read_u64_le(data_hdr, 4);
    let data_size = data_chunk_size.saturating_sub(12);
    let data_offset = 92u64; // 28 + 52 + 12

    Ok(DsfInfo {
        channels,
        sample_rate,
        bits_per_sample,
        total_samples,
        block_size,
        data_offset,
        data_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid DSF header in memory.
    fn build_dsf_header(
        channels: u32,
        sample_rate: u32,
        total_samples: u64,
        block_size: u32,
        dsd_data: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();

        // --- DSD Chunk (28 bytes) ---
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes()); // chunk size
        let total_file_size = 28 + 52 + 12 + dsd_data.len() as u64;
        buf.extend_from_slice(&total_file_size.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // metadata offset (none)

        // --- Format Chunk (52 bytes) ---
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes()); // chunk size
        buf.extend_from_slice(&1u32.to_le_bytes()); // format version
        buf.extend_from_slice(&0u32.to_le_bytes()); // format ID = DSD raw
        buf.extend_from_slice(&2u32.to_le_bytes()); // channel type (stereo)
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        buf.extend_from_slice(&total_samples.to_le_bytes());
        buf.extend_from_slice(&block_size.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved

        // --- Data Chunk header (12 bytes) + data ---
        buf.extend_from_slice(b"data");
        let data_chunk_size = 12 + dsd_data.len() as u64;
        buf.extend_from_slice(&data_chunk_size.to_le_bytes());
        buf.extend_from_slice(dsd_data);

        buf
    }

    #[test]
    fn parse_valid_dsf_header() {
        let dsd_data = vec![0u8; 8192]; // 2 channels * 4096 block
        let buf = build_dsf_header(2, 2_822_400, 32768, 4096, &dsd_data);

        let info = parse_dsf_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 2_822_400);
        assert_eq!(info.bits_per_sample, 1);
        assert_eq!(info.total_samples, 32768);
        assert_eq!(info.block_size, 4096);
        assert_eq!(info.data_offset, 92);
        assert_eq!(info.data_size, 8192);
    }

    #[test]
    fn parse_dsf_dsd128() {
        let dsd_data = vec![0u8; 8192];
        let buf = build_dsf_header(2, 5_644_800, 65536, 4096, &dsd_data);

        let info = parse_dsf_from_bytes(&buf).unwrap();
        assert_eq!(info.sample_rate, 5_644_800);
        assert_eq!(info.total_samples, 65536);
    }

    #[test]
    fn parse_dsf_bad_magic() {
        let mut buf = build_dsf_header(2, 2_822_400, 32768, 4096, &[0u8; 8192]);
        buf[0] = b'X';
        let result = parse_dsf_from_bytes(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing 'DSD ' magic"));
    }

    #[test]
    fn parse_dsf_bad_fmt_magic() {
        let mut buf = build_dsf_header(2, 2_822_400, 32768, 4096, &[0u8; 8192]);
        buf[28] = b'X'; // corrupt "fmt " to "Xmt "
        let result = parse_dsf_from_bytes(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("fmt"));
    }

    #[test]
    fn parse_dsf_bad_data_magic() {
        let mut buf = build_dsf_header(2, 2_822_400, 32768, 4096, &[0u8; 8192]);
        buf[80] = b'X'; // corrupt "data" to "Xata"
        let result = parse_dsf_from_bytes(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("data"));
    }

    #[test]
    fn parse_dsf_too_short() {
        let result = parse_dsf_from_bytes(&[0u8; 50]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_dsf_mono() {
        let dsd_data = vec![0u8; 4096]; // 1 channel * 4096 block
        let buf = build_dsf_header(1, 2_822_400, 32768, 4096, &dsd_data);
        let info = parse_dsf_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 1);
    }
}
