pub fn build_wav_header(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * (bit_depth as u32 / 8);
    let block_align = channels * (bit_depth / 8);
    let data_size: u32 = 0x7FFF_FFFF;
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
}
