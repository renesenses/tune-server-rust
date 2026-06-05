use oaat_core::format::AudioFormat;

/// Parsed audio stream header info.
pub(super) struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub format: AudioFormat,
    /// Duration in ms derived from header, or 0 if unknown.
    pub duration_ms: u64,
    /// DSD multiplier (64, 128, 256, 512) if DSD format.
    pub dsd_rate: Option<u16>,
    /// Byte offset where audio data starts in the original file/stream.
    /// Used for seek byte-range calculations.
    pub data_offset: usize,
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
    if buf.len() >= 92 && &buf[..4] == b"DSD " {
        return parse_dsf(buf);
    }
    None
}

fn parse_wav(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    let channels = u16::from_le_bytes([buf[22], buf[23]]);
    let sample_rate = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let bits_per_sample = u16::from_le_bytes([buf[34], buf[35]]);

    let mut offset = 12;
    let mut data_size: u64 = 0;
    let mut found_data = false;
    let mut audio_data_offset = 44usize;
    while offset + 8 <= buf.len() {
        let chunk_id = &buf[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);
        if chunk_id == b"data" {
            data_size = chunk_size as u64;
            audio_data_offset = offset + 8;
            buf.drain(..offset + 8);
            found_data = true;
            break;
        }
        offset += 8 + chunk_size as usize;
    }
    if !found_data {
        audio_data_offset = 44;
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
        dsd_rate: None,
        data_offset: audio_data_offset,
    })
}

/// Parse FLAC STREAMINFO metadata block.
fn parse_flac(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    if buf.len() < 42 {
        return None;
    }
    let block_type = buf[4] & 0x7F;
    if block_type != 0 {
        return None;
    }
    let block_len = ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | (buf[7] as usize);
    if block_len < 34 || buf.len() < 8 + block_len {
        return None;
    }

    let si = &buf[8..8 + 34];
    let sr_hi = ((si[10] as u32) << 12) | ((si[11] as u32) << 4) | ((si[12] as u32) >> 4);
    let sample_rate = sr_hi;
    let channels = ((si[12] >> 1) & 0x07) as u16 + 1;
    let bps_hi = ((si[12] & 0x01) as u16) << 4;
    let bps_lo = ((si[13] >> 4) & 0x0F) as u16;
    let bits_per_sample = (bps_hi | bps_lo) + 1;

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

    Some(StreamInfo {
        sample_rate,
        channels,
        bits_per_sample,
        format: AudioFormat::Flac,
        duration_ms,
        dsd_rate: None,
        data_offset: 0, // FLAC header kept in buffer, seek not supported
    })
}

/// Parse DSF (DSD Stream File) header.
/// DSF layout: DSD chunk (28 bytes) + fmt chunk (52 bytes) + data chunk header (12 bytes)
/// Audio data starts at offset 92 for standard DSF files.
fn parse_dsf(buf: &mut Vec<u8>) -> Option<StreamInfo> {
    if buf.len() < 92 || &buf[..4] != b"DSD " {
        return None;
    }
    // Verify fmt chunk
    if &buf[28..32] != b"fmt " {
        return None;
    }

    let channels = u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]) as u16;
    let sample_rate = u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]);
    let sample_count = u64::from_le_bytes([
        buf[64], buf[65], buf[66], buf[67], buf[68], buf[69], buf[70], buf[71],
    ]);

    let dsd_rate = dsd_rate_from_sample_rate(sample_rate);
    let duration_ms = if sample_rate > 0 && sample_count > 0 {
        sample_count * 1000 / sample_rate as u64
    } else {
        0
    };

    // Verify data chunk
    if &buf[80..84] != b"data" {
        return None;
    }

    let audio_data_offset = 92;
    buf.drain(..audio_data_offset);

    Some(StreamInfo {
        sample_rate,
        channels,
        bits_per_sample: 1,
        format: AudioFormat::DsdU8,
        duration_ms,
        dsd_rate,
        data_offset: audio_data_offset,
    })
}

pub(super) fn dsd_rate_from_sample_rate(sr: u32) -> Option<u16> {
    match sr {
        2_822_400 => Some(64),
        5_644_800 => Some(128),
        11_289_600 => Some(256),
        22_579_200 => Some(512),
        _ => None,
    }
}

pub(super) fn bits_to_format(bits: u16) -> AudioFormat {
    match bits {
        16 => AudioFormat::PcmS16le,
        24 => AudioFormat::PcmS24le,
        32 => AudioFormat::PcmS32le,
        _ => AudioFormat::PcmS16le,
    }
}

pub(super) fn format_rate_display(rate: u32, bits: u16, format: AudioFormat) -> String {
    if format.is_dsd() {
        if let Some(mult) = dsd_rate_from_sample_rate(rate) {
            return format!("DSD{mult}");
        }
        return format!("DSD {rate}Hz");
    }
    let khz = rate as f64 / 1000.0;
    let prefix = if format == AudioFormat::Flac {
        "FLAC"
    } else {
        "PCM"
    };
    if khz.fract() == 0.0 {
        format!("{prefix} {bits}/{}", khz as u32)
    } else {
        format!("{prefix} {bits}/{khz:.1}")
    }
}

/// Convert a FLAC file to WAV using ffmpeg, return WAV bytes.
pub(super) fn decode_flac_to_pcm(flac_path: &str) -> Option<Vec<u8>> {
    let tmp = format!("/tmp/oaat-{}.wav", std::process::id());
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", flac_path, "-acodec", "pcm_s24le", &tmp])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let data = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    data
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
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        buf.resize(buf.len() + data_size as usize, 0);
        buf
    }

    fn make_dsf(sample_rate: u32, channels: u32, sample_count: u64, data_size: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        // DSD chunk (28 bytes)
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes()); // chunk size
        buf.extend_from_slice(&(92u64 + data_size as u64).to_le_bytes()); // total file size
        buf.extend_from_slice(&0u64.to_le_bytes()); // metadata offset
        // fmt chunk (52 bytes)
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes()); // chunk size
        buf.extend_from_slice(&1u32.to_le_bytes()); // format version
        buf.extend_from_slice(&0u32.to_le_bytes()); // format ID (DSD raw)
        buf.extend_from_slice(&2u32.to_le_bytes()); // channel type (stereo)
        buf.extend_from_slice(&channels.to_le_bytes()); // channel count
        buf.extend_from_slice(&sample_rate.to_le_bytes()); // sample rate
        buf.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        buf.extend_from_slice(&sample_count.to_le_bytes()); // sample count per channel
        buf.extend_from_slice(&4096u32.to_le_bytes()); // block size per channel
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        // data chunk header (12 bytes)
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(12u64 + data_size as u64).to_le_bytes()); // data chunk size
        // Audio data
        buf.resize(buf.len() + data_size as usize, 0xAA);
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
        assert!(si.dsd_rate.is_none());
        assert_eq!(si.data_offset, 44);
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
        let mut buf = Vec::new();
        buf.extend_from_slice(b"fLaC");
        buf.push(0x80);
        buf.extend_from_slice(&[0x00, 0x00, 0x22]);
        buf.extend_from_slice(&[0x10, 0x00]);
        buf.extend_from_slice(&[0x10, 0x00]);
        buf.extend_from_slice(&[0x00, 0x00, 0x00]);
        buf.extend_from_slice(&[0x00, 0x00, 0x00]);
        buf.push(0x0A);
        buf.push(0xC4);
        buf.push(0x42);
        buf.push(0xF0);
        buf.extend_from_slice(&[0x00, 0x06, 0xBA, 0xA8]);
        buf.extend_from_slice(&[0u8; 16]);
        buf.resize(128, 0);

        let si = detect_and_parse(&mut buf).expect("should parse FLAC");
        assert_eq!(si.sample_rate, 44100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bits_per_sample, 16);
        assert_eq!(si.format, AudioFormat::Flac);
        assert_eq!(si.duration_ms, 10000);
        assert!(si.dsd_rate.is_none());
    }

    #[test]
    fn dsf_dsd64() {
        // DSD64: 2,822,400 Hz, stereo, 10 seconds = 28,224,000 samples
        let sample_count = 2_822_400u64 * 10;
        let data_size = (sample_count * 2 / 8) as u32; // 2 channels, 1 bit per sample, /8 for bytes
        let mut buf = make_dsf(2_822_400, 2, sample_count, data_size);
        let si = detect_and_parse(&mut buf).expect("should parse DSF");
        assert_eq!(si.sample_rate, 2_822_400);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bits_per_sample, 1);
        assert_eq!(si.format, AudioFormat::DsdU8);
        assert_eq!(si.dsd_rate, Some(64));
        assert_eq!(si.duration_ms, 10000);
        assert_eq!(si.data_offset, 92);
    }

    #[test]
    fn dsf_dsd128() {
        let sample_count = 5_644_800u64 * 5;
        let data_size = (sample_count * 2 / 8) as u32;
        let mut buf = make_dsf(5_644_800, 2, sample_count, data_size);
        let si = detect_and_parse(&mut buf).expect("should parse DSF");
        assert_eq!(si.sample_rate, 5_644_800);
        assert_eq!(si.dsd_rate, Some(128));
        assert_eq!(si.duration_ms, 5000);
    }

    #[test]
    fn dsf_drains_header() {
        let data_size = 8192u32;
        let mut buf = make_dsf(2_822_400, 2, 2_822_400, data_size);
        let original_len = buf.len();
        let _ = detect_and_parse(&mut buf).unwrap();
        assert_eq!(buf.len(), data_size as usize);
        assert!(buf.len() < original_len);
        assert_eq!(buf[0], 0xAA); // verify it's audio data, not header
    }

    #[test]
    fn unknown_format_returns_none() {
        let mut buf = vec![0xFF; 128];
        assert!(detect_and_parse(&mut buf).is_none());
    }

    #[test]
    fn format_display() {
        assert_eq!(
            format_rate_display(44100, 16, AudioFormat::PcmS16le),
            "PCM 16/44.1"
        );
        assert_eq!(
            format_rate_display(48000, 24, AudioFormat::PcmS24le),
            "PCM 24/48"
        );
        assert_eq!(
            format_rate_display(192000, 24, AudioFormat::PcmS24le),
            "PCM 24/192"
        );
        assert_eq!(
            format_rate_display(96000, 24, AudioFormat::Flac),
            "FLAC 24/96"
        );
        assert_eq!(
            format_rate_display(44100, 16, AudioFormat::Flac),
            "FLAC 16/44.1"
        );
    }

    #[test]
    fn format_display_dsd() {
        assert_eq!(
            format_rate_display(2_822_400, 1, AudioFormat::DsdU8),
            "DSD64"
        );
        assert_eq!(
            format_rate_display(5_644_800, 1, AudioFormat::DsdU16le),
            "DSD128"
        );
        assert_eq!(
            format_rate_display(11_289_600, 1, AudioFormat::DsdU32le),
            "DSD256"
        );
    }

    #[test]
    fn bits_to_format_mapping() {
        assert_eq!(bits_to_format(16), AudioFormat::PcmS16le);
        assert_eq!(bits_to_format(24), AudioFormat::PcmS24le);
        assert_eq!(bits_to_format(32), AudioFormat::PcmS32le);
        assert_eq!(bits_to_format(8), AudioFormat::PcmS16le);
    }

    #[test]
    fn dsd_rate_mapping() {
        assert_eq!(dsd_rate_from_sample_rate(2_822_400), Some(64));
        assert_eq!(dsd_rate_from_sample_rate(5_644_800), Some(128));
        assert_eq!(dsd_rate_from_sample_rate(11_289_600), Some(256));
        assert_eq!(dsd_rate_from_sample_rate(22_579_200), Some(512));
        assert_eq!(dsd_rate_from_sample_rate(44100), None);
    }
}
