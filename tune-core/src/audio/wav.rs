/// Build a 44-byte WAV header. When `duration_ms` is provided, the header
/// contains the correct data size so DLNA renderers don't need to probe
/// the stream end. Falls back to max u32 for unknown-length streams.
pub fn build_wav_header(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    build_wav_header_with_duration(channels, sample_rate, bit_depth, None)
}

pub fn build_wav_header_with_duration(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    duration_ms: Option<u64>,
) -> [u8; 44] {
    let data_size: u32 = if let Some(dur) = duration_ms {
        let bytes = dur * sample_rate as u64 * channels as u64 * (bit_depth as u64 / 8) / 1000;
        bytes.min(0x7FFF_FFFF) as u32
    } else {
        0x7FFF_FFFF
    };
    build_wav_header_with_data_size(channels, sample_rate, bit_depth, data_size)
}

/// Build a 44-byte WAV header for an INFINITE live stream (internet radio).
///
/// Uses the "indeterminate length" convention (`RIFF` and `data` chunk sizes
/// both set to `0xFFFF_FFFF`) instead of a finite size. An FFmpeg/libavformat
/// (`Lavf`) DLNA renderer treats `0xFFFF_FFFF` as an unbounded stream and keeps
/// reading until the connection closes, whereas the previous finite
/// `0x7FFF_FFFF` (~2 GiB) size makes it treat the transcoded radio as a bounded
/// PCM file: it fills its ~64 MiB read-ahead cache and then stops/reconnects
/// after ~6 minutes (FIP cutoff, .15 zone_id=10, `Lavf/58.45.100`).
pub fn build_wav_header_streaming(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    // Both the RIFF and the data chunk sizes are set to 0xFFFF_FFFF (the
    // canonical "unknown/streaming length" marker). build_wav_header_with_data_size
    // would wrap the RIFF size to `data_size + 36`, so patch it to 0xFFFF_FFFF here.
    let mut header = build_wav_header_with_data_size(channels, sample_rate, bit_depth, 0xFFFF_FFFF);
    header[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    header
}

/// Build a 44-byte WAV header with an exact `data` chunk size, for complete
/// (non-streaming) WAV files where the full PCM length is known upfront.
pub fn build_wav_header_with_data_size(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    data_size: u32,
) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * (bit_depth as u32 / 8);
    let block_align = channels * (bit_depth / 8);
    let file_size: u32 = data_size.wrapping_add(36);

    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&file_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bit_depth.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_structure() {
        let h = build_wav_header(2, 44100, 16);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(h.len(), 44);
        // byte_rate = 44100 * 2 * 2 = 176400
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 176400);
    }

    #[test]
    fn wav_header_mono() {
        let h = build_wav_header(1, 44100, 16);
        let channels = u16::from_le_bytes([h[22], h[23]]);
        assert_eq!(channels, 1);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 88200); // 44100 * 1 * 2
        let block_align = u16::from_le_bytes([h[32], h[33]]);
        assert_eq!(block_align, 2); // 1 * 2
    }

    #[test]
    fn wav_header_24bit() {
        let h = build_wav_header(2, 96000, 24);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 576000); // 96000 * 2 * 3
        let block_align = u16::from_le_bytes([h[32], h[33]]);
        assert_eq!(block_align, 6); // 2 * 3
        let bit_depth = u16::from_le_bytes([h[34], h[35]]);
        assert_eq!(bit_depth, 24);
    }

    #[test]
    fn wav_header_hires() {
        let h = build_wav_header(2, 192000, 24);
        let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
        assert_eq!(sample_rate, 192000);
    }

    #[test]
    fn wav_header_pcm_format() {
        let h = build_wav_header(2, 44100, 16);
        let format = u16::from_le_bytes([h[20], h[21]]);
        assert_eq!(format, 1); // PCM
    }

    #[test]
    fn wav_header_fmt_chunk_size() {
        let h = build_wav_header(2, 44100, 16);
        let chunk_size = u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
        assert_eq!(chunk_size, 16);
    }

    #[test]
    fn wav_header_data_size() {
        let h = build_wav_header(2, 44100, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 0x7FFF_FFFF);
    }

    #[test]
    fn wav_header_with_known_duration() {
        // 3 minutes of 44100/16/2 = 180s * 176400 bytes/s = 31752000 bytes
        let h = build_wav_header_with_duration(2, 44100, 16, Some(180_000));
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 180 * 44100 * 2 * 2);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(riff_size, data_size + 36);
    }

    #[test]
    fn wav_header_without_duration_uses_max() {
        let h = build_wav_header_with_duration(2, 44100, 16, None);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 0x7FFF_FFFF);
    }

    #[test]
    fn wav_header_streaming_uses_indeterminate_length() {
        // Live radio: both RIFF and data chunk sizes must be 0xFFFF_FFFF so a
        // Lavf renderer treats the stream as unbounded and reads until close,
        // instead of stopping at the finite 0x7FFF_FFFF (~2 GiB) size.
        let h = build_wav_header_streaming(2, 48000, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 0xFFFF_FFFF);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(riff_size, 0xFFFF_FFFF);
        // Format fields still reflect the true stream: 48000/16/2.
        let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
        assert_eq!(sample_rate, 48000);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 48000 * 2 * 2);
    }

    #[test]
    fn wav_header_duration_hires() {
        // 4:16.487 of 96000/24/2
        let h = build_wav_header_with_duration(2, 96000, 24, Some(256_487));
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        let expected = 256_487u64 * 96000 * 2 * 3 / 1000;
        assert_eq!(data_size, expected as u32);
    }
}
