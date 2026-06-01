use oaat_core::format::AudioFormat;

/// Parsed audio stream header info.
pub(super) struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub format: AudioFormat,
    /// Duration in ms derived from header, or 0 if unknown.
    pub duration_ms: u64,
}

/// Detect stream format from the first bytes and parse header.
/// Drains the header from `buf`, leaving only audio data.
pub(super) fn detect_and_parse(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    if buf.len() >= 44 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
        return parse_wav(buf);
    }
    if buf.len() >= 42 && &buf[..4] == b"fLaC" {
        return parse_flac(buf);
    }
    None
}

fn parse_wav(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    let channels = u16::from_le_bytes([buf[22], buf[23]]);
    let sample_rate = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let bits_per_sample = u16::from_le_bytes([buf[34], buf[35]]);

    let mut data_offset = 12;
    let mut data_size: u64 = 0;
    let mut found_data = false;
    while data_offset + 8 <= buf.len() {
        let chunk_id = &buf[data_offset..data_offset + 4];
        let chunk_size = u32::from_le_bytes([
            buf[data_offset + 4],
            buf[data_offset + 5],
            buf[data_offset + 6],
            buf[data_offset + 7],
        ]);
        if chunk_id == b"data" {
            data_size = chunk_size as u64;
            buf.drain(..data_offset + 8);
            found_data = true;
            break;
        }
        data_offset += 8 + chunk_size as usize;
    }
    if !found_data {
        buf.drain(..44);
    }

    let bytes_per_frame = (bits_per_sample as u64 / 8) * channels as u64;
    let duration_ms = if bytes_per_frame > 0 && sample_rate > 0 && data_size > 0 {
        data_size * 1000 / (sample_rate as u64 * bytes_per_frame)
    } else {
        0
    };

    Some(StreamInfo {
        sample_rate,
        channels,
        bits_per_sample,
        format: bits_to_format(bits_per_sample),
        duration_ms,
    })
}

/// Parse FLAC STREAMINFO metadata block.
/// The FLAC header is: "fLaC" + metadata blocks. STREAMINFO is always the first block.
/// STREAMINFO (34 bytes): min_block(2) + max_block(2) + min_frame(3) + max_frame(3) +
///   sample_rate(20 bits) + channels-1(3 bits) + bps-1(5 bits) + total_samples(36 bits) + md5(16)
fn parse_flac(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    if buf.len() < 42 {
        return None;
    }
    // Byte 4: metadata block header — type (7 bits, should be 0 = STREAMINFO)
    let block_type = buf[4] & 0x7F;
    if block_type != 0 {
        return None;
    }
    let block_len = ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | (buf[7] as usize);
    if block_len < 34 || buf.len() < 8 + block_len {
        return None;
    }

    // STREAMINFO starts at byte 8
    let si = &buf[8..8 + 34];
    // Bytes 10-13 of STREAMINFO: sample_rate (20 bits) | channels-1 (3 bits) | bps-1 (5 bits)
    let sr_hi = ((si[10] as u32) << 12) | ((si[11] as u32) << 4) | ((si[12] as u32) >> 4);
    let sample_rate = sr_hi;
    let channels = ((si[12] >> 1) & 0x07) as u16 + 1;
    let bps_hi = ((si[12] & 0x01) as u16) << 4;
    let bps_lo = ((si[13] >> 4) & 0x0F) as u16;
    let bits_per_sample = (bps_hi | bps_lo) + 1;

    // Total samples (36 bits): lower 4 bits of si[13] + si[14..18]
    let total_lo = ((si[13] & 0x0F) as u64) << 32;
    let total_hi = ((si[14] as u64) << 24)
        | ((si[15] as u64) << 16)
        | ((si[16] as u64) << 8)
        | (si[17] as u64);
    let total_samples = total_lo | total_hi;
    let duration_ms = if sample_rate > 0 && total_samples > 0 {
        total_samples * 1000 / sample_rate as u64
    } else {
        0
    };

    // Don't drain FLAC header — the endpoint needs the full FLAC stream
    // (header + frames) for symphonia to decode.
    // We keep the buffer as-is; audio packets include raw FLAC data.

    Some(StreamInfo {
        sample_rate,
        channels,
        bits_per_sample,
        format: AudioFormat::Flac,
        duration_ms,
    })
}

pub(super) fn bits_to_format(bits: u16) -> AudioFormat {
    match bits {
        16 => AudioFormat::PcmS16le,
        24 => AudioFormat::PcmS24le,
        32 => AudioFormat::PcmS32le,
        _ => AudioFormat::PcmS16le,
    }
}

pub(super) fn format_rate_display(rate: u32, bits: u16, is_flac: bool) -> String {
    let khz = rate as f64 / 1000.0;
    let prefix = if is_flac { "FLAC" } else { "PCM" };
    if khz.fract() == 0.0 {
        format!("{prefix} {bits}/{}", khz as u32)
    } else {
        format!("{prefix} {bits}/{khz:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wav(sample_rate: u32, channels: u16, bits: u16, data_size: u32) -> Vec<u8> {
        let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
        let block_align = channels * bits / 8;
        let file_size = 36 + data_size;
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&file_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        // fmt chunk
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits.to_le_bytes());
        // data chunk
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        buf.resize(buf.len() + data_size as usize, 0);
        buf
    }

    #[test]
    fn wav_44100_16_stereo() {
        let mut buf = make_wav(44100, 2, 16, 44100 * 4);
        let si = detect_and_parse(&mut buf).expect("should parse WAV");
        assert_eq!(si.sample_rate, 44100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bits_per_sample, 16);
        assert_eq!(si.format, AudioFormat::PcmS16le);
        assert_eq!(si.duration_ms, 1000);
    }

    #[test]
    fn wav_192000_24_stereo() {
        let mut buf = make_wav(192000, 2, 24, 192000 * 6 * 5);
        let si = detect_and_parse(&mut buf).expect("should parse WAV");
        assert_eq!(si.sample_rate, 192000);
        assert_eq!(si.bits_per_sample, 24);
        assert_eq!(si.format, AudioFormat::PcmS24le);
        assert_eq!(si.duration_ms, 5000);
    }

    #[test]
    fn wav_drains_header() {
        let data_size = 1024u32;
        let mut buf = make_wav(48000, 2, 16, data_size);
        let original_len = buf.len();
        let _ = detect_and_parse(&mut buf).unwrap();
        assert_eq!(buf.len(), data_size as usize);
        assert!(buf.len() < original_len);
    }

    #[test]
    fn flac_streaminfo() {
        // Minimal FLAC with STREAMINFO: 44100 Hz, 2ch, 16-bit, 441000 samples (10s)
        let mut buf = Vec::new();
        buf.extend_from_slice(b"fLaC");
        // Metadata block header: last=1 (0x80), type=0 (STREAMINFO), length=34
        buf.push(0x80);
        buf.extend_from_slice(&[0x00, 0x00, 0x22]); // length 34
        // STREAMINFO: min_block(2) + max_block(2) + min_frame(3) + max_frame(3) = 10 bytes
        buf.extend_from_slice(&[0x10, 0x00]); // min_block_size = 4096
        buf.extend_from_slice(&[0x10, 0x00]); // max_block_size = 4096
        buf.extend_from_slice(&[0x00, 0x00, 0x00]); // min_frame_size
        buf.extend_from_slice(&[0x00, 0x00, 0x00]); // max_frame_size
        // sample_rate (20 bits) = 44100 = 0x0AC44
        // channels-1 (3 bits) = 1 (stereo)
        // bps-1 (5 bits) = 15 (16-bit)
        // byte[10]: sr bits 19..12 = 0x0A
        buf.push(0x0A);
        // byte[11]: sr bits 11..4 = 0xC4
        buf.push(0xC4);
        // byte[12]: sr bits 3..0 (0x4) | channels-1 (001) | bps-1 upper bit (0)
        // bps-1=15=0b01111, upper bit=0
        // 0100_001_0 = 0x42
        buf.push(0x42);
        // byte[13]: lower 4 bits of bps-1 (1111=0xF) | upper 4 bits of total_samples
        // bps_lo = 0xF, total_samples upper 4 bits = 0 → 0xF0
        // Wait, bps-1 = 15 = 0b01111. Upper bit is in byte[12], lower 4 bits = 0b1111 = 0xF
        buf.push(0xF0);
        // total_samples (remaining 32 bits) = 441000 = 0x0006BAA8
        buf.extend_from_slice(&[0x00, 0x06, 0xBA, 0xA8]);
        // MD5 (16 bytes)
        buf.extend_from_slice(&[0u8; 16]);
        // Pad to 128 bytes for detect_and_parse
        buf.resize(128, 0);

        let si = detect_and_parse(&mut buf).expect("should parse FLAC");
        assert_eq!(si.sample_rate, 44100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bits_per_sample, 16);
        assert_eq!(si.format, AudioFormat::Flac);
        assert_eq!(si.duration_ms, 10000);
    }

    #[test]
    fn unknown_format_returns_none() {
        let mut buf = vec![0xFF; 128];
        assert!(detect_and_parse(&mut buf).is_none());
    }

    #[test]
    fn format_display() {
        assert_eq!(format_rate_display(44100, 16, false), "PCM 16/44.1");
        assert_eq!(format_rate_display(48000, 24, false), "PCM 24/48");
        assert_eq!(format_rate_display(192000, 24, false), "PCM 24/192");
        assert_eq!(format_rate_display(96000, 24, true), "FLAC 24/96");
        assert_eq!(format_rate_display(44100, 16, true), "FLAC 16/44.1");
    }

    #[test]
    fn bits_to_format_mapping() {
        assert_eq!(bits_to_format(16), AudioFormat::PcmS16le);
        assert_eq!(bits_to_format(24), AudioFormat::PcmS24le);
        assert_eq!(bits_to_format(32), AudioFormat::PcmS32le);
        assert_eq!(bits_to_format(8), AudioFormat::PcmS16le); // fallback
    }
}
