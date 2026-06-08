//! Integration tests for native audio decoders.
//!
//! These tests generate tiny valid audio files programmatically in a temp
//! directory, decode them through `decode_to_pcm()`, and verify the output:
//! correct sample rate, channel count, non-empty samples, reasonable duration.

use std::io::Write;
use std::path::Path;

use tempfile::TempDir;

// ── File generators ──────────────────────────────────────────────────────

/// Create a 1-second stereo 16-bit 44100 Hz WAV file using hound.
fn create_test_wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..44100u32 {
        // Simple sine-ish pattern so samples aren't all zero
        let val = ((i % 100) as i16).wrapping_mul(100);
        writer.write_sample(val).unwrap(); // L
        writer.write_sample(-val).unwrap(); // R
    }
    writer.finalize().unwrap();
}

/// Create a 1-second stereo 16-bit 44100 Hz AIFF file manually.
///
/// AIFF uses big-endian throughout:
/// - FORM header (12 bytes): "FORM" + size + "AIFF"
/// - COMM chunk (26 bytes): channels, frames, bits, sample rate (80-bit extended)
/// - SSND chunk: offset(4) + blockSize(4) + PCM data
fn create_test_aiff(path: &Path) {
    let channels: u16 = 2;
    let num_frames: u32 = 44100;
    let bits_per_sample: u16 = 16;
    let bytes_per_frame = channels as u32 * (bits_per_sample as u32 / 8);
    let pcm_data_size = num_frames * bytes_per_frame;

    // COMM chunk: 18 bytes of content
    let mut comm = Vec::new();
    comm.extend_from_slice(b"COMM");
    comm.extend_from_slice(&18u32.to_be_bytes());
    comm.extend_from_slice(&(channels as i16).to_be_bytes());
    comm.extend_from_slice(&num_frames.to_be_bytes());
    comm.extend_from_slice(&(bits_per_sample as i16).to_be_bytes());
    // 44100 Hz in 80-bit IEEE 754 extended
    comm.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

    // SSND chunk: 8-byte sub-header + PCM data
    let ssnd_content_size = 8 + pcm_data_size;
    let mut ssnd = Vec::new();
    ssnd.extend_from_slice(b"SSND");
    ssnd.extend_from_slice(&ssnd_content_size.to_be_bytes());
    ssnd.extend_from_slice(&0u32.to_be_bytes()); // offset
    ssnd.extend_from_slice(&0u32.to_be_bytes()); // blockSize

    // PCM data: stereo 16-bit big-endian with a simple pattern
    for i in 0..num_frames {
        let val = ((i % 100) as i16).wrapping_mul(100);
        ssnd.extend_from_slice(&val.to_be_bytes()); // L
        ssnd.extend_from_slice(&(-val).to_be_bytes()); // R
    }

    // FORM header
    let body_size = comm.len() + ssnd.len();
    let mut form = Vec::new();
    form.extend_from_slice(b"FORM");
    form.extend_from_slice(&((body_size as u32) + 4).to_be_bytes()); // +4 for "AIFF"
    form.extend_from_slice(b"AIFF");
    form.extend_from_slice(&comm);
    form.extend_from_slice(&ssnd);

    std::fs::write(path, &form).unwrap();
}

/// Create a minimal valid DSF file (DSD64 stereo, ~0.01 seconds).
///
/// DSF is little-endian throughout:
/// - DSD chunk (28 bytes): "DSD " + chunk_size=28 + file_size + metadata_offset=0
/// - fmt chunk (52 bytes): version=1, format_id=0, channel_type=2, channels=2,
///   sample_rate=2822400, bits=1, sample_count, block_size=4096
/// - data chunk: "data" + chunk_size + DSD blocks
///
/// We generate one super-block (one block per channel, each 4096 bytes).
/// At DSD64 (2822400 Hz), 4096 bytes = 32768 DSD samples per channel.
/// Duration = 32768 / 2822400 = ~0.0116 seconds.
fn create_test_dsf(path: &Path) {
    let channels: u32 = 2;
    let sample_rate: u32 = 2_822_400;
    let block_size: u32 = 4096;
    let total_samples: u64 = 32768; // 4096 bytes * 8 bits
    let dsd_data_size = (block_size * channels) as usize; // one super-block

    // DSD data: alternating pattern (0x69 = 01101001)
    let dsd_data: Vec<u8> = vec![0x69; dsd_data_size];

    let data_chunk_size = 12u64 + dsd_data_size as u64;
    let total_file_size = 28u64 + 52u64 + data_chunk_size;

    let mut buf = Vec::new();

    // DSD Chunk (28 bytes)
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&total_file_size.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes()); // no metadata

    // Format Chunk (52 bytes)
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // format version
    buf.extend_from_slice(&0u32.to_le_bytes()); // format ID = DSD raw
    buf.extend_from_slice(&2u32.to_le_bytes()); // channel type (stereo)
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&block_size.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // Data Chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_chunk_size.to_le_bytes());
    buf.extend_from_slice(&dsd_data);

    std::fs::write(path, &buf).unwrap();
}

/// Create a minimal valid DFF file (DSD64 stereo, ~0.01 seconds).
///
/// DFF (DSDIFF) is big-endian, IFF-based:
/// - FRM8 header: "FRM8" + size(u64) + "DSD "
/// - PROP chunk with sub-chunks: FS, CHNL, CMPR
/// - DSD sound data chunk
fn create_test_dff(path: &Path) {
    let channels: u16 = 2;
    let sample_rate: u32 = 2_822_400;
    // 4096 bytes of interleaved DSD data (2048 bytes per channel, 16384 samples per ch)
    let dsd_data_size: u64 = 4096;
    let dsd_data: Vec<u8> = vec![0x55; dsd_data_size as usize]; // 01010101 pattern

    // Build PROP chunk content
    let mut prop_content = Vec::new();
    prop_content.extend_from_slice(b"SND ");

    // FS sub-chunk
    prop_content.extend_from_slice(b"FS  ");
    prop_content.extend_from_slice(&4u64.to_be_bytes());
    prop_content.extend_from_slice(&sample_rate.to_be_bytes());

    // CHNL sub-chunk
    prop_content.extend_from_slice(b"CHNL");
    prop_content.extend_from_slice(&2u64.to_be_bytes());
    prop_content.extend_from_slice(&channels.to_be_bytes());

    // CMPR sub-chunk
    prop_content.extend_from_slice(b"CMPR");
    prop_content.extend_from_slice(&4u64.to_be_bytes());
    prop_content.extend_from_slice(b"DSD ");

    let prop_chunk_size = prop_content.len() as u64;

    // FRM8 content: 4 (form type) + 12 (PROP header) + prop_content + 12 (DSD header) + dsd_data
    let frm8_content_size = 4 + 12 + prop_chunk_size + 12 + dsd_data_size;

    let mut buf = Vec::new();

    // FRM8 header
    buf.extend_from_slice(b"FRM8");
    buf.extend_from_slice(&frm8_content_size.to_be_bytes());
    buf.extend_from_slice(b"DSD ");

    // PROP chunk
    buf.extend_from_slice(b"PROP");
    buf.extend_from_slice(&prop_chunk_size.to_be_bytes());
    buf.extend_from_slice(&prop_content);

    // DSD sound data chunk
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&dsd_data_size.to_be_bytes());
    buf.extend_from_slice(&dsd_data);

    std::fs::write(path, &buf).unwrap();
}

/// Create a minimal valid APE file header (header only, no encoded audio data).
///
/// This generates a valid APE v3990 header with seek table, but the actual
/// encoded frame data is missing, so `decode_ape_to_pcm()` should return an
/// error rather than panicking.
fn create_test_ape_header(path: &Path) {
    let channels: u16 = 2;
    let sample_rate: u32 = 44100;
    let bits_per_sample: u16 = 16;
    let total_frames: u32 = 1;
    let blocks_per_frame: u32 = 73728;
    let final_frame_blocks: u32 = 44100;
    let compression: u16 = 2000; // Normal

    let mut buf = Vec::new();

    // Descriptor (52 bytes)
    buf.extend_from_slice(b"MAC "); // magic
    buf.extend_from_slice(&3990u16.to_le_bytes()); // version
    buf.extend_from_slice(&0u16.to_le_bytes()); // padding
    buf.extend_from_slice(&52u32.to_le_bytes()); // descriptor_bytes
    buf.extend_from_slice(&24u32.to_le_bytes()); // header_bytes
    let seek_table_bytes = total_frames * 4;
    buf.extend_from_slice(&seek_table_bytes.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // header_data_bytes
    buf.extend_from_slice(&0u32.to_le_bytes()); // ape_frame_data_bytes
    buf.extend_from_slice(&0u32.to_le_bytes()); // ape_frame_data_bytes_high
    buf.extend_from_slice(&0u32.to_le_bytes()); // terminating_data_bytes
    buf.extend_from_slice(&[0u8; 16]); // file_md5

    // Header (24 bytes at offset 52)
    buf.extend_from_slice(&compression.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // format_flags
    buf.extend_from_slice(&blocks_per_frame.to_le_bytes());
    buf.extend_from_slice(&final_frame_blocks.to_le_bytes());
    buf.extend_from_slice(&total_frames.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());

    // Seek table (dummy entries pointing past the file)
    let base_offset = 52 + 24 + seek_table_bytes;
    for i in 0..total_frames {
        buf.extend_from_slice(&(base_offset + i * 1024).to_le_bytes());
    }

    std::fs::write(path, &buf).unwrap();
}

/// Create a minimal WavPack file header (header only, no sub-blocks/bitstream).
///
/// Like APE, this generates a valid block header but without the sub-block
/// data needed for actual decoding, so `decode_wavpack_to_pcm()` should
/// return an error rather than panicking.
fn create_test_wavpack_header(path: &Path) {
    let mut buf = Vec::new();

    // Block header (32 bytes)
    buf.extend_from_slice(b"wvpk"); // magic
    // block_size: 24 bytes of header fields after magic+size
    buf.extend_from_slice(&24u32.to_le_bytes());
    buf.extend_from_slice(&0x0410u16.to_le_bytes()); // version
    buf.push(0); // track_no
    buf.push(0); // index_no
    buf.extend_from_slice(&44100u32.to_le_bytes()); // total_samples
    buf.extend_from_slice(&0u32.to_le_bytes()); // block_index
    buf.extend_from_slice(&1024u32.to_le_bytes()); // block_samples

    // flags: 16-bit (bps_minus_1=1), stereo, 44100 (sr_index=9), initial+final block
    let flags: u32 = 1 // bytes_per_sample - 1 = 1 (16-bit)
        | (9 << 23)    // sample rate index 9 = 44100
        | (1 << 11)    // initial block
        | (1 << 12); // final block
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // crc

    std::fs::write(path, &buf).unwrap();
}

// ── Integration tests ────────────────────────────────────────────────────

#[test]
fn decode_wav_integration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wav");
    create_test_wav(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    assert_eq!(result.sample_rate, 44100);
    assert_eq!(result.channels, 2);
    assert!(!result.samples_i32.is_empty(), "WAV should produce samples");
    assert!(
        result.duration_s > 0.9 && result.duration_s < 1.1,
        "duration should be ~1s, got {}",
        result.duration_s
    );
    // Verify sample count: 44100 frames * 2 channels = 88200
    assert_eq!(result.samples_i32.len(), 88200);
}

#[test]
fn decode_aiff_integration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.aiff");
    create_test_aiff(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    assert_eq!(result.sample_rate, 44100);
    assert_eq!(result.channels, 2);
    assert!(
        !result.samples_i32.is_empty(),
        "AIFF should produce samples"
    );
    assert!(
        result.duration_s > 0.9 && result.duration_s < 1.1,
        "duration should be ~1s, got {}",
        result.duration_s
    );
    assert_eq!(result.samples_i32.len(), 88200);
}

#[test]
fn decode_aiff_sample_values() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.aiff");
    create_test_aiff(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    // First frame: i=0, val = (0 % 100) * 100 = 0
    assert_eq!(result.samples_i32[0], 0, "first L sample should be 0");
    assert_eq!(result.samples_i32[1], 0, "first R sample should be 0");

    // Second frame: i=1, val = (1 % 100) * 100 = 100
    assert_eq!(result.samples_i32[2], 100, "second L sample should be 100");
    assert_eq!(
        result.samples_i32[3], -100,
        "second R sample should be -100"
    );
}

#[test]
fn decode_dsf_integration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.dsf");
    create_test_dsf(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    // DSD64 at 2822400 Hz gets decimated to 176400 Hz
    assert_eq!(result.sample_rate, 176400);
    assert_eq!(result.channels, 2);
    assert!(
        !result.samples_i32.is_empty(),
        "DSF decode should produce PCM samples"
    );
    assert!(
        result.duration_s > 0.0,
        "DSF should have positive duration, got {}",
        result.duration_s
    );
}

#[test]
fn decode_dff_integration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.dff");
    create_test_dff(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    // DSD64 at 2822400 Hz gets decimated to 176400 Hz
    assert_eq!(result.sample_rate, 176400);
    assert_eq!(result.channels, 2);
    assert!(
        !result.samples_i32.is_empty(),
        "DFF decode should produce PCM samples"
    );
    assert!(
        result.duration_s > 0.0,
        "DFF should have positive duration, got {}",
        result.duration_s
    );
}

#[test]
fn decode_wav_with_seek() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wav");
    create_test_wav(&path);

    let full =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    let seeked =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.5, 0.0)
            .unwrap();

    assert!(
        seeked.samples_i32.len() < full.samples_i32.len(),
        "seeked decode should have fewer samples ({} vs {})",
        seeked.samples_i32.len(),
        full.samples_i32.len()
    );
    assert!(
        !seeked.samples_i32.is_empty(),
        "seeked decode should still have samples"
    );
}

#[test]
fn decode_wav_with_duration_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wav");
    create_test_wav(&path);

    let full =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    let half =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.5)
            .unwrap();

    assert!(
        half.samples_i32.len() < full.samples_i32.len(),
        "limited decode should have fewer samples ({} vs {})",
        half.samples_i32.len(),
        full.samples_i32.len()
    );
    assert!(
        !half.samples_i32.is_empty(),
        "limited decode should still have samples"
    );
    // Half-second at 44100 Hz stereo = ~44100 samples
    assert!(
        half.samples_i32.len() <= 44200,
        "0.5s limit should produce ~44100 samples, got {}",
        half.samples_i32.len()
    );
}

#[test]
fn decode_aiff_with_seek() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.aiff");
    create_test_aiff(&path);

    let full =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    let seeked =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.5, 0.0)
            .unwrap();

    assert!(
        seeked.samples_i32.len() < full.samples_i32.len(),
        "seeked AIFF should have fewer samples"
    );
}

#[test]
fn decode_aiff_with_duration_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.aiff");
    create_test_aiff(&path);

    let full =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();

    let half =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.5)
            .unwrap();

    assert!(
        half.samples_i32.len() < full.samples_i32.len(),
        "limited AIFF decode should have fewer samples"
    );
}

#[test]
fn decode_aiff_seek_past_end() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.aiff");
    create_test_aiff(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 999.0, 0.0)
            .unwrap();

    assert!(
        result.samples_i32.is_empty(),
        "seeking past end should produce empty samples"
    );
}

#[test]
fn decode_nonexistent_file() {
    let result =
        tune_core::audio::decode::decode_to_pcm("/nonexistent/path/file.wav", None, None, 0.0, 0.0);
    assert!(result.is_err(), "decoding nonexistent file should fail");
}

#[test]
fn decode_nonexistent_aiff() {
    let result = tune_core::audio::decode::decode_to_pcm(
        "/nonexistent/path/file.aiff",
        None,
        None,
        0.0,
        0.0,
    );
    assert!(result.is_err(), "decoding nonexistent AIFF should fail");
}

#[test]
fn decode_nonexistent_dsf() {
    let result =
        tune_core::audio::decode::decode_to_pcm("/nonexistent/path/file.dsf", None, None, 0.0, 0.0);
    assert!(result.is_err(), "decoding nonexistent DSF should fail");
}

#[test]
fn decode_nonexistent_dff() {
    let result =
        tune_core::audio::decode::decode_to_pcm("/nonexistent/path/file.dff", None, None, 0.0, 0.0);
    assert!(result.is_err(), "decoding nonexistent DFF should fail");
}

#[test]
fn decode_nonexistent_wavpack() {
    let result =
        tune_core::audio::decode::decode_to_pcm("/nonexistent/path/file.wv", None, None, 0.0, 0.0);
    assert!(result.is_err(), "decoding nonexistent WavPack should fail");
}

#[test]
fn decode_nonexistent_ape() {
    let result =
        tune_core::audio::decode::decode_to_pcm("/nonexistent/path/file.ape", None, None, 0.0, 0.0);
    assert!(result.is_err(), "decoding nonexistent APE should fail");
}

// ── can_decode_native coverage ───────────────────────────────────────────

#[test]
fn can_decode_all_native_formats() {
    for ext in &[
        "flac", "mp3", "wav", "m4a", "aac", "alac", "ogg", "aiff", "aif", "dsf", "dff", "wv", "ape",
    ] {
        assert!(
            tune_core::audio::decode::can_decode_native(&format!("test.{ext}")),
            "can_decode_native should return true for .{ext}"
        );
    }
}

#[test]
fn cannot_decode_unsupported_formats() {
    for ext in &["txt", "pdf", "jpg", "opus", "wma", "mid"] {
        assert!(
            !tune_core::audio::decode::can_decode_native(&format!("test.{ext}")),
            "can_decode_native should return false for .{ext}"
        );
    }
}

#[test]
fn can_decode_case_insensitive() {
    // Path extension comparison should be case-insensitive
    assert!(tune_core::audio::decode::can_decode_native("test.FLAC"));
    assert!(tune_core::audio::decode::can_decode_native("test.Wav"));
    assert!(tune_core::audio::decode::can_decode_native("test.AIFF"));
    assert!(tune_core::audio::decode::can_decode_native("test.DSF"));
}

// ── Header-only parse tests for complex formats ──────────────────────────

#[test]
fn parse_wavpack_header_only() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wv");
    create_test_wavpack_header(&path);

    let info = tune_core::audio::wavpack::parse_wavpack(path.to_str().unwrap()).unwrap();

    assert_eq!(info.sample_rate, 44100);
    assert_eq!(info.channels, 2);
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(info.total_samples, 44100);
}

#[test]
fn decode_wavpack_truncated_graceful() {
    // A WavPack file with only a header and no sub-block data should fail
    // gracefully (return an error), not panic.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wv");
    create_test_wavpack_header(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);

    // It should either error or return empty samples — either is acceptable
    // as long as it does not panic.
    match result {
        Ok(_decoded) => {
            // If it "succeeds", it should have produced no useful data
            // (an empty block with no bitstream sub-block yields 0 samples)
        }
        Err(_) => {
            // Expected: truncated file causes an error
        }
    }
}

#[test]
fn parse_ape_header_only() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ape");
    create_test_ape_header(&path);

    let info = tune_core::audio::ape::parse_ape(path.to_str().unwrap()).unwrap();

    assert_eq!(info.version, 3990);
    assert_eq!(info.compression_level, 2000);
    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, 44100);
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(info.total_frames, 1);
    assert_eq!(info.total_samples, 44100);
}

#[test]
fn decode_ape_truncated_graceful() {
    // An APE file with only a header but no encoded frame data should fail
    // gracefully, not panic.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ape");
    create_test_ape_header(&path);

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);

    // Should return an error, not panic
    match result {
        Ok(_) => {}  // Unlikely but acceptable
        Err(_) => {} // Expected
    }
}

// ── DSF/DFF parser tests via public API ──────────────────────────────────

#[test]
fn parse_dsf_generated_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.dsf");
    create_test_dsf(&path);

    let info = tune_core::audio::dsf::parse_dsf(path.to_str().unwrap()).unwrap();

    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, 2_822_400);
    assert_eq!(info.bits_per_sample, 1);
    assert_eq!(info.total_samples, 32768);
    assert_eq!(info.block_size, 4096);
}

#[test]
fn parse_dff_generated_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.dff");
    create_test_dff(&path);

    let info = tune_core::audio::dff::parse_dff(path.to_str().unwrap()).unwrap();

    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, 2_822_400);
    assert_eq!(info.compression, "DSD ");
    assert_eq!(info.data_size, 4096);
}

// ── Corrupt / invalid file tests ─────────────────────────────────────────

#[test]
fn decode_corrupt_wav_graceful() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupt.wav");
    // Write random garbage
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"this is not a valid wav file at all").unwrap();

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
    assert!(result.is_err(), "corrupt WAV should return an error");
}

#[test]
fn decode_corrupt_aiff_graceful() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupt.aiff");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"FORM\x00\x00\x00\x04XXXX").unwrap();

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
    assert!(result.is_err(), "corrupt AIFF should return an error");
}

#[test]
fn decode_corrupt_dsf_graceful() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupt.dsf");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"DSD XXXX not a real dsf file").unwrap();

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
    assert!(result.is_err(), "corrupt DSF should return an error");
}

#[test]
fn decode_empty_file_graceful() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.wav");
    std::fs::File::create(&path).unwrap();

    let result =
        tune_core::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
    assert!(result.is_err(), "empty file should return an error");
}

// ── DSF with target sample rate ──────────────────────────────────────────

#[test]
fn decode_dsf_with_target_sample_rate() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.dsf");
    create_test_dsf(&path);

    // Request specific output rate (88200 instead of default 176400)
    let result = tune_core::audio::decode::decode_to_pcm(
        path.to_str().unwrap(),
        Some(88200),
        None,
        0.0,
        0.0,
    )
    .unwrap();

    assert_eq!(result.sample_rate, 88200);
    assert_eq!(result.channels, 2);
    assert!(!result.samples_i32.is_empty());
}
