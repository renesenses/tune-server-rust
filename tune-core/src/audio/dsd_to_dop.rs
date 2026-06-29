/// DSD over PCM (DoP) encoder.
///
/// Packs raw DSD bitstream data into 24-bit PCM frames with DoP marker
/// bytes in the top 8 bits. This allows DSD playback through PCM-only
/// audio interfaces (WASAPI, ASIO, CoreAudio).
///
/// DoP frame layout (24-bit LE per channel):
///   byte 0: DSD bits [7:0]  (low byte of 16 DSD bits)
///   byte 1: DSD bits [15:8] (high byte of 16 DSD bits)
///   byte 2: marker (0x05 or 0xFA, alternating per frame)
///
/// Sample rates:
///   DSD64  (2.8224 MHz) → 176.4 kHz DoP
///   DSD128 (5.6448 MHz) → 352.8 kHz DoP
///   DSD256 (11.2896 MHz) → 705.6 kHz DoP

pub struct DsdToDoP {
    channels: usize,
    lsb_first: bool,
    frame_count: u64,
}

impl DsdToDoP {
    pub fn new(channels: usize, lsb_first: bool) -> Self {
        Self {
            channels,
            lsb_first,
            frame_count: 0,
        }
    }

    pub fn dop_rate(dsd_rate: u32) -> u32 {
        dsd_rate / 16
    }

    /// Feed a chunk of byte-interleaved DSD data and return 24-bit LE DoP PCM.
    ///
    /// Input: byte-interleaved DSD (ch0_b0, ch1_b0, ch0_b1, ch1_b1, ...)
    /// Each byte = 8 DSD bits. We need 16 bits (2 bytes) per channel per DoP frame.
    /// So we consume `2 * channels` bytes per DoP frame.
    pub fn feed(&mut self, dsd_data: &[u8]) -> Vec<u8> {
        let bytes_per_frame = 2 * self.channels;
        let num_frames = dsd_data.len() / bytes_per_frame;
        // 3 bytes per channel per frame (24-bit)
        let mut out = Vec::with_capacity(num_frames * 3 * self.channels);

        for frame_idx in 0..num_frames {
            let marker = if self.frame_count % 2 == 0 {
                0x05u8
            } else {
                0xFAu8
            };

            for ch in 0..self.channels {
                let offset = frame_idx * bytes_per_frame + ch;
                let b0 = dsd_data[offset]; // first 8 DSD bits
                let b1 = dsd_data[offset + self.channels]; // next 8 DSD bits

                // DoP expects MSB-first DSD. DSF is LSB-first → reverse bits.
                let (d0, d1) = if self.lsb_first {
                    (reverse_bits(b0), reverse_bits(b1))
                } else {
                    (b0, b1)
                };

                // 24-bit LE: [low_dsd, high_dsd, marker]
                out.push(d1);
                out.push(d0);
                out.push(marker);
            }

            self.frame_count += 1;
        }

        out
    }
}

fn reverse_bits(b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        r |= ((b >> i) & 1) << (7 - i);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_rate_dsd64() {
        assert_eq!(DsdToDoP::dop_rate(2_822_400), 176_400);
    }

    #[test]
    fn dop_rate_dsd128() {
        assert_eq!(DsdToDoP::dop_rate(5_644_800), 352_800);
    }

    #[test]
    fn marker_alternates() {
        let mut dop = DsdToDoP::new(1, false);
        // 4 bytes = 2 frames for mono (2 bytes per frame)
        let data = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let out = dop.feed(&data);
        // Frame 0: marker 0x05, frame 1: marker 0xFA
        assert_eq!(out.len(), 6); // 2 frames × 3 bytes
        assert_eq!(out[2], 0x05); // first frame marker
        assert_eq!(out[5], 0xFA); // second frame marker
    }

    #[test]
    fn stereo_output_size() {
        let mut dop = DsdToDoP::new(2, false);
        // 8 bytes = 2 frames for stereo (4 bytes per frame = 2 bytes × 2 channels)
        let data = vec![0; 8];
        let out = dop.feed(&data);
        // 2 frames × 2 channels × 3 bytes = 12 bytes
        assert_eq!(out.len(), 12);
    }

    #[test]
    fn lsb_first_reversal() {
        let mut dop = DsdToDoP::new(1, true); // LSB-first (DSF)
        let data = vec![0b10000000, 0b00000001]; // 1 frame
        let out = dop.feed(&data);
        // 0b10000000 reversed = 0b00000001
        // 0b00000001 reversed = 0b10000000
        assert_eq!(out[1], 0b00000001); // high byte = reversed b0
        assert_eq!(out[0], 0b10000000); // low byte = reversed b1
    }
}
