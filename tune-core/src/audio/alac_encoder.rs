//! Native ALAC (Apple Lossless) file encoder (#1526).
//!
//! Wraps Apple's reference encoder (vendored under `vendor/alac`,
//! Apache-2.0, compiled by build.rs — same precedent as rusqlite
//! `bundled`) and writes a minimal single-track `.m4a` container around
//! the packets. Replaces the converter's ffmpeg subprocess for ALAC.
//!
//! ALAC is lossless: the round-trip test decodes the produced file with
//! the project's own decoder (symphonia isomp4 + alac) and requires the
//! PCM to match the source exactly.

use std::os::raw::c_void;

/// Frames per ALAC packet — Apple's `kALACDefaultFrameSize`.
const FRAMES_PER_PACKET: u32 = 4096;

unsafe extern "C" {
    fn tune_alac_encoder_create(sample_rate: f64, channels: u32, bit_depth: u32) -> *mut c_void;
    fn tune_alac_encode_packet(
        handle: *mut c_void,
        sample_rate: f64,
        channels: u32,
        bit_depth: u32,
        input: *const u8,
        output: *mut u8,
        io_bytes: *mut i32,
    ) -> i32;
    fn tune_alac_magic_cookie_size(handle: *mut c_void, channels: u32) -> u32;
    fn tune_alac_magic_cookie(handle: *mut c_void, buf: *mut u8, io_size: *mut u32);
    fn tune_alac_encoder_destroy(handle: *mut c_void);
}

/// RAII guard so every exit path releases the C++ encoder.
struct Encoder(*mut c_void);

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { tune_alac_encoder_destroy(self.0) };
    }
}

/// Encode interleaved i32 samples (at `bit_depth` ∈ {16, 24, 32}) into a
/// complete `.m4a` byte stream.
pub fn encode_alac_m4a(
    samples: &[i32],
    bit_depth: u16,
    channels: u16,
    sample_rate: u32,
) -> Result<Vec<u8>, String> {
    if !matches!(bit_depth, 16 | 24 | 32) {
        return Err(format!(
            "alac: profondeur {bit_depth} bits non prise en charge (16/24/32)"
        ));
    }
    if channels == 0 || channels > 8 {
        return Err(format!("alac: {channels} canaux non pris en charge"));
    }
    let ch = channels as usize;
    let total_frames = samples.len() / ch;
    if total_frames == 0 {
        return Err("alac: aucun échantillon".into());
    }

    let enc =
        unsafe { tune_alac_encoder_create(sample_rate as f64, channels as u32, bit_depth as u32) };
    if enc.is_null() {
        return Err("alac: initialisation de l'encodeur refusée".into());
    }
    let enc = Encoder(enc);

    // Magic cookie — the decoder config that goes in the 'alac' box.
    let mut cookie_size = unsafe { tune_alac_magic_cookie_size(enc.0, channels as u32) };
    let mut cookie = vec![0u8; cookie_size as usize];
    unsafe { tune_alac_magic_cookie(enc.0, cookie.as_mut_ptr(), &mut cookie_size) };
    cookie.truncate(cookie_size as usize);

    // Pack samples to little-endian signed bytes at the source depth.
    let bytes_per_sample = (bit_depth / 8) as usize;
    let mut pcm = Vec::with_capacity(samples.len() * bytes_per_sample);
    for &s in samples {
        let b = s.to_le_bytes();
        pcm.extend_from_slice(&b[..bytes_per_sample]);
    }

    // Encode packet by packet (4096 frames each, last one partial).
    let packet_in_bytes = FRAMES_PER_PACKET as usize * ch * bytes_per_sample;
    let mut out_buf = vec![0u8; packet_in_bytes + 8192]; // slack for escape headers
    let mut packets: Vec<Vec<u8>> = Vec::new();
    for chunk in pcm.chunks(packet_in_bytes) {
        let mut io_bytes = chunk.len() as i32;
        let status = unsafe {
            tune_alac_encode_packet(
                enc.0,
                sample_rate as f64,
                channels as u32,
                bit_depth as u32,
                chunk.as_ptr(),
                out_buf.as_mut_ptr(),
                &mut io_bytes,
            )
        };
        if status != 0 {
            return Err(format!("alac: encode a rendu {status}"));
        }
        packets.push(out_buf[..io_bytes as usize].to_vec());
    }

    Ok(super::m4a::mux(&super::m4a::M4aTrack {
        sample_entry_kind: *b"alac",
        codec_box: super::m4a::full_box(b"alac", 0, 0, &cookie),
        packets: &packets,
        frames_per_packet: FRAMES_PER_PACKET,
        total_frames: total_frames as u64,
        sample_rate,
        channels,
        bit_depth,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-noise + sine mix — exercises the predictor
    /// harder than a pure sine.
    fn test_signal(frames: usize, ch: usize, amplitude: i32) -> Vec<i32> {
        let mut state = 0x1234_5678u32;
        let mut out = Vec::with_capacity(frames * ch);
        for n in 0..frames {
            for c in 0..ch {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 16) as i32 % (amplitude / 8).max(1);
                let sine = ((2.0 * std::f64::consts::PI * (220.0 + 110.0 * c as f64) * n as f64
                    / 44100.0)
                    .sin()
                    * amplitude as f64) as i32;
                out.push(sine + noise);
            }
        }
        out
    }

    fn round_trip(bit_depth: u16, channels: u16, sample_rate: u32, frames: usize) {
        let amplitude = match bit_depth {
            16 => 16_000,
            24 => 4_000_000,
            _ => 1_000_000_000,
        };
        let samples = test_signal(frames, channels as usize, amplitude);
        let m4a = encode_alac_m4a(&samples, bit_depth, channels, sample_rate).expect("encode");

        let dir = std::env::temp_dir().join(format!("tune-alac-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rt-{bit_depth}-{channels}-{sample_rate}.m4a"));
        std::fs::write(&path, &m4a).unwrap();

        let decoded =
            crate::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, f64::MAX)
                .expect("decode back");
        let _ = std::fs::remove_file(&path);

        assert_eq!(decoded.sample_rate, sample_rate);
        assert_eq!(decoded.channels as u16, channels);
        assert_eq!(decoded.bit_depth, bit_depth);
        // Lossless means LOSSLESS: exact PCM equality, sample for sample.
        assert_eq!(
            decoded.samples_i32.len(),
            samples.len(),
            "frame count drifted"
        );
        assert_eq!(decoded.samples_i32, samples, "PCM is not bit-exact");
    }

    #[test]
    fn round_trip_16_bit_stereo_is_bit_exact() {
        // 1.7 s → several full packets + a partial tail.
        round_trip(16, 2, 44100, 75_000);
    }

    #[test]
    fn round_trip_24_bit_stereo_is_bit_exact() {
        round_trip(24, 2, 96000, 50_000);
    }

    #[test]
    fn round_trip_mono_is_bit_exact() {
        round_trip(16, 1, 48000, 30_000);
    }

    #[test]
    fn exact_packet_boundary_has_no_tail_garbage() {
        // Exactly 2 packets of 4096 frames — the "last partial" entry in
        // stts must still describe a real packet, not a phantom.
        round_trip(16, 2, 44100, 8192);
    }

    #[test]
    fn rejects_unsupported_depth() {
        let s = vec![0i32; 1000];
        assert!(encode_alac_m4a(&s, 20, 2, 44100).is_err());
    }
}
