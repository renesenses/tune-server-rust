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
    let block_len =
        ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | (buf[7] as usize);
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
    let bits_per_sample = bps_hi | bps_lo + 1;

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
