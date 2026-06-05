//! Native AIFF/AIFC parser and PCM decoder.
//!
//! Parses the IFF container (FORM + COMM + SSND chunks) and extracts
//! interleaved PCM samples as i16.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use tracing::debug;

use super::decode::DecodedAudio;

/// Parsed AIFF/AIFC file metadata.
pub struct AiffInfo {
    pub channels: u16,
    pub num_frames: u32,
    pub bits_per_sample: u16,
    pub sample_rate: f64,
    pub is_aifc: bool,
    /// Compression type for AIFC files (e.g. "NONE", "sowt", "fl32", "fl64").
    /// `None` for plain AIFF (always uncompressed big-endian PCM).
    pub compression: Option<String>,
    /// Absolute file offset where the PCM sample data begins (after SSND
    /// offset/blockSize header).
    pub data_offset: u64,
    /// Size of the PCM sample data in bytes.
    pub data_size: u64,
}

/// Convert an 80-bit IEEE 754 extended-precision float (big-endian) to f64.
///
/// AIFF stores the sample rate in this format inside the COMM chunk.
fn extended_to_f64(bytes: &[u8; 10]) -> f64 {
    let sign = (bytes[0] >> 7) & 1;
    let exponent = (((bytes[0] & 0x7F) as u16) << 8) | (bytes[1] as u16);
    let mantissa = u64::from_be_bytes([
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
    ]);

    if exponent == 0 && mantissa == 0 {
        return 0.0;
    }

    let f = (mantissa as f64) / (1u64 << 63) as f64;
    let value = f * 2.0_f64.powi(exponent as i32 - 16383);
    if sign == 1 { -value } else { value }
}

/// Read exactly `N` bytes from a reader into a fixed-size array.
fn read_bytes<const N: usize>(r: &mut impl Read) -> Result<[u8; N], String> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read {N} bytes: {e}"))?;
    Ok(buf)
}

/// Parse an AIFF or AIFC file and return its metadata.
pub fn parse_aiff(path: &str) -> Result<AiffInfo, String> {
    let mut f = File::open(path).map_err(|e| format!("open: {e}"))?;

    // --- FORM header (12 bytes) ---
    let magic = read_bytes::<4>(&mut f)?;
    if &magic != b"FORM" {
        return Err("not an IFF/AIFF file (missing FORM)".into());
    }
    let _file_size = u32::from_be_bytes(read_bytes::<4>(&mut f)?);
    let form_type = read_bytes::<4>(&mut f)?;
    let is_aifc = match &form_type {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err(format!("unsupported form type: {:?}", form_type)),
    };

    let mut channels: Option<u16> = None;
    let mut num_frames: Option<u32> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut sample_rate: Option<f64> = None;
    let mut compression: Option<String> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_size: Option<u64> = None;

    // --- iterate chunks ---
    loop {
        let chunk_id = match read_bytes::<4>(&mut f) {
            Ok(id) => id,
            Err(_) => break, // EOF while reading next chunk header — done
        };
        let chunk_size = u32::from_be_bytes(match read_bytes::<4>(&mut f) {
            Ok(b) => b,
            Err(_) => break,
        });

        let chunk_start = f
            .stream_position()
            .map_err(|e| format!("stream_position: {e}"))?;

        match &chunk_id {
            b"COMM" => {
                let ch = i16::from_be_bytes(read_bytes::<2>(&mut f)?);
                let nf = u32::from_be_bytes(read_bytes::<4>(&mut f)?);
                let bps = i16::from_be_bytes(read_bytes::<2>(&mut f)?);
                let sr_bytes = read_bytes::<10>(&mut f)?;

                channels = Some(ch as u16);
                num_frames = Some(nf);
                bits_per_sample = Some(bps as u16);
                sample_rate = Some(extended_to_f64(&sr_bytes));

                // AIFC extends COMM with compression type + name
                if is_aifc && chunk_size > 18 {
                    let comp_type = read_bytes::<4>(&mut f)?;
                    compression = Some(String::from_utf8_lossy(&comp_type).to_string());
                    // Skip the pascal string (compression name) — we don't need it
                }
            }
            b"SSND" => {
                let offset_field = u32::from_be_bytes(read_bytes::<4>(&mut f)?);
                let _block_size = u32::from_be_bytes(read_bytes::<4>(&mut f)?);
                // PCM data starts after the 8-byte SSND sub-header + offset field
                let pcm_start = chunk_start + 8 + offset_field as u64;
                let pcm_size = chunk_size as u64 - 8 - offset_field as u64;
                data_offset = Some(pcm_start);
                data_size = Some(pcm_size);
            }
            _ => {
                // Skip unknown chunks
            }
        }

        // Advance to next chunk (IFF chunks are padded to even byte boundaries)
        let padded_size = chunk_size as u64 + (chunk_size as u64 % 2);
        f.seek(SeekFrom::Start(chunk_start + padded_size))
            .map_err(|e| format!("seek past chunk: {e}"))?;
    }

    let channels = channels.ok_or("missing COMM chunk (no channels)")?;
    let num_frames = num_frames.ok_or("missing COMM chunk (no frames)")?;
    let bits_per_sample = bits_per_sample.ok_or("missing COMM chunk (no bit depth)")?;
    let sample_rate = sample_rate.ok_or("missing COMM chunk (no sample rate)")?;
    let data_offset = data_offset.ok_or("missing SSND chunk")?;
    let data_size = data_size.ok_or("missing SSND chunk")?;

    Ok(AiffInfo {
        channels,
        num_frames,
        bits_per_sample,
        sample_rate,
        is_aifc,
        compression,
        data_offset,
        data_size,
    })
}

/// Decode an AIFF/AIFC file to interleaved i16 PCM samples.
///
/// Supports seek (in seconds) and duration limiting.
pub fn decode_aiff_to_pcm(
    path: &str,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let info = parse_aiff(path)?;

    let is_little_endian = matches!(info.compression.as_deref(), Some("sowt"));
    let is_float = matches!(info.compression.as_deref(), Some("fl32") | Some("fl64"));

    // Validate compression — we only handle uncompressed and common AIFC types
    if let Some(ref comp) = info.compression {
        match comp.as_str() {
            "NONE" | "sowt" | "fl32" | "fl64" => {}
            other => return Err(format!("unsupported AIFC compression: {other}")),
        }
    }

    let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
    let frame_size = bytes_per_sample * info.channels as u32;

    // Compute seek offset in frames
    let seek_frames = if seek_s > 0.0 {
        (seek_s * info.sample_rate).round() as u64
    } else {
        0
    };

    // Compute max frames to read
    let available_frames = if seek_frames < info.num_frames as u64 {
        info.num_frames as u64 - seek_frames
    } else {
        0
    };
    let max_frames = if max_duration_s > 0.0 {
        let limit = (max_duration_s * info.sample_rate).round() as u64;
        available_frames.min(limit)
    } else {
        available_frames
    };

    if max_frames == 0 {
        return Ok(DecodedAudio {
            samples: Vec::new(),
            sample_rate: info.sample_rate.round() as u32,
            channels: info.channels as u32,
            duration_s: 0.0,
        });
    }

    // Open file and seek to PCM data start + seek offset
    let mut f = File::open(path).map_err(|e| format!("open: {e}"))?;
    let pcm_offset = info.data_offset + seek_frames * frame_size as u64;
    f.seek(SeekFrom::Start(pcm_offset))
        .map_err(|e| format!("seek to PCM data: {e}"))?;

    // Read the raw PCM bytes
    let total_bytes = max_frames * frame_size as u64;
    let mut raw = vec![0u8; total_bytes as usize];
    f.read_exact(&mut raw)
        .map_err(|e| format!("read PCM data: {e}"))?;

    // Convert to interleaved i16
    let total_samples = (max_frames * info.channels as u64) as usize;
    let mut samples = Vec::with_capacity(total_samples);

    if is_float {
        // Float formats
        match info.bits_per_sample {
            32 => {
                for chunk in raw.chunks_exact(4) {
                    let f_val = if is_little_endian {
                        f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                    } else {
                        f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                    };
                    let clamped = f_val.max(-1.0).min(1.0);
                    samples.push((clamped * i16::MAX as f32) as i16);
                }
            }
            64 => {
                for chunk in raw.chunks_exact(8) {
                    let f_val = if is_little_endian {
                        f64::from_le_bytes([
                            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6],
                            chunk[7],
                        ])
                    } else {
                        f64::from_be_bytes([
                            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6],
                            chunk[7],
                        ])
                    };
                    let clamped = f_val.max(-1.0).min(1.0);
                    samples.push((clamped * i16::MAX as f64) as i16);
                }
            }
            _ => {
                return Err(format!(
                    "unsupported float bit depth: {}",
                    info.bits_per_sample
                ));
            }
        }
    } else if is_little_endian {
        // AIFC "sowt" — little-endian PCM (same layout as WAV)
        match info.bits_per_sample {
            8 => {
                for &b in &raw {
                    // 8-bit AIFF is unsigned
                    let signed = b as i16 - 128;
                    samples.push(signed << 8);
                }
            }
            16 => {
                for chunk in raw.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
            }
            24 => {
                for chunk in raw.chunks_exact(3) {
                    let val = i32::from_le_bytes([0, chunk[0], chunk[1], chunk[2]]);
                    // Sign-extend from 24 bits
                    let val = (val << 8) >> 8;
                    samples.push((val >> 8) as i16);
                }
            }
            32 => {
                for chunk in raw.chunks_exact(4) {
                    let val = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    samples.push((val >> 16) as i16);
                }
            }
            _ => return Err(format!("unsupported bit depth: {}", info.bits_per_sample)),
        }
    } else {
        // Standard AIFF — big-endian signed PCM
        match info.bits_per_sample {
            8 => {
                // 8-bit AIFF PCM is signed (unlike WAV which is unsigned)
                for &b in &raw {
                    samples.push((b as i8 as i16) << 8);
                }
            }
            16 => {
                for chunk in raw.chunks_exact(2) {
                    samples.push(i16::from_be_bytes([chunk[0], chunk[1]]));
                }
            }
            24 => {
                for chunk in raw.chunks_exact(3) {
                    // Build a 32-bit value from 3 big-endian bytes, sign-extend
                    let val = i32::from_be_bytes([chunk[0], chunk[1], chunk[2], 0]);
                    samples.push((val >> 16) as i16);
                }
            }
            32 => {
                for chunk in raw.chunks_exact(4) {
                    let val = i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    samples.push((val >> 16) as i16);
                }
            }
            _ => return Err(format!("unsupported bit depth: {}", info.bits_per_sample)),
        }
    }

    let duration_s = max_frames as f64 / info.sample_rate;

    debug!(
        file = path,
        samples = samples.len(),
        rate = info.sample_rate,
        channels = info.channels,
        bits = info.bits_per_sample,
        aifc = info.is_aifc,
        duration_s,
        "decoded_aiff_native"
    );

    Ok(DecodedAudio {
        samples,
        sample_rate: info.sample_rate.round() as u32,
        channels: info.channels as u32,
        duration_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> String {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures");
        p.push(name);
        p.to_string_lossy().to_string()
    }

    // --- IEEE 754 extended precision tests ---

    #[test]
    fn extended_44100() {
        // 44100 Hz in 80-bit extended: exponent=0x400E (16398), mantissa=0xAC44...
        let bytes: [u8; 10] = [0x40, 0x0E, 0xAC, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let rate = extended_to_f64(&bytes);
        assert!((rate - 44100.0).abs() < 0.01, "expected 44100, got {rate}");
    }

    #[test]
    fn extended_48000() {
        // 48000 Hz: exponent = 16398 (0x400E), mantissa = 0xBB80_0000_0000_0000
        let bytes: [u8; 10] = [0x40, 0x0E, 0xBB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let rate = extended_to_f64(&bytes);
        assert!((rate - 48000.0).abs() < 0.01, "expected 48000, got {rate}");
    }

    #[test]
    fn extended_96000() {
        // 96000 Hz: exponent = 16399 (0x400F), mantissa = 0xBB80_0000_0000_0000
        let bytes: [u8; 10] = [0x40, 0x0F, 0xBB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let rate = extended_to_f64(&bytes);
        assert!((rate - 96000.0).abs() < 0.01, "expected 96000, got {rate}");
    }

    #[test]
    fn extended_192000() {
        // 192000 Hz: exponent = 16400 (0x4010), mantissa = 0xBB80_0000_0000_0000
        let bytes: [u8; 10] = [0x40, 0x10, 0xBB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let rate = extended_to_f64(&bytes);
        assert!(
            (rate - 192000.0).abs() < 0.01,
            "expected 192000, got {rate}"
        );
    }

    #[test]
    fn extended_zero() {
        let bytes = [0u8; 10];
        assert_eq!(extended_to_f64(&bytes), 0.0);
    }

    // --- Big-endian PCM conversion tests ---

    #[test]
    fn pcm_16bit_be_conversion() {
        // 16-bit big-endian: 0x7FFF = i16::MAX, 0x8001 = -32767
        let raw = vec![0x7F, 0xFF, 0x80, 0x01, 0x00, 0x00];
        let mut samples = Vec::new();
        for chunk in raw.chunks_exact(2) {
            samples.push(i16::from_be_bytes([chunk[0], chunk[1]]));
        }
        assert_eq!(samples, vec![i16::MAX, -32767, 0]);
    }

    #[test]
    fn pcm_24bit_be_to_i16() {
        // 24-bit big-endian: 0x7FFFFF -> should map to ~i16::MAX
        let raw = vec![0x7F, 0xFF, 0xFF];
        let val = i32::from_be_bytes([raw[0], raw[1], raw[2], 0]);
        let sample = (val >> 16) as i16;
        assert_eq!(sample, i16::MAX);
    }

    #[test]
    fn pcm_8bit_signed_conversion() {
        // 8-bit AIFF is signed: 0x7F = +127, 0x80 = -128, 0x00 = 0
        let raw = vec![0x7Fu8, 0x80, 0x00];
        let mut samples = Vec::new();
        for &b in &raw {
            samples.push((b as i8 as i16) << 8);
        }
        assert_eq!(samples[0], 127 * 256); // +127 scaled
        assert_eq!(samples[1], -128 * 256); // -128 scaled
        assert_eq!(samples[2], 0); // silence
    }

    // --- Parser tests with real fixture ---

    #[test]
    fn parse_aiff_fixture() {
        let path = fixture_path("test.aiff");
        let info = parse_aiff(&path).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert!(!info.is_aifc);
        assert!(info.compression.is_none());
        assert!(
            (info.sample_rate - 44100.0).abs() < 0.01,
            "expected 44100, got {}",
            info.sample_rate
        );
        assert!(info.num_frames > 0, "should have frames");
        assert!(info.data_size > 0, "should have data");
    }

    #[test]
    fn decode_aiff_fixture() {
        let path = fixture_path("test.aiff");
        let result = decode_aiff_to_pcm(&path, 0.0, 0.0).unwrap();
        assert!(!result.samples.is_empty(), "should produce samples");
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 0.9 && result.duration_s < 1.1,
            "duration should be ~1s, got {}",
            result.duration_s
        );
    }

    #[test]
    fn decode_aiff_with_seek() {
        let path = fixture_path("test.aiff");
        let full = decode_aiff_to_pcm(&path, 0.0, 0.0).unwrap();
        let seeked = decode_aiff_to_pcm(&path, 0.5, 0.0).unwrap();
        assert!(
            seeked.samples.len() < full.samples.len(),
            "seeked should have fewer samples ({} vs {})",
            seeked.samples.len(),
            full.samples.len()
        );
    }

    #[test]
    fn decode_aiff_with_duration_limit() {
        let path = fixture_path("test.aiff");
        let full = decode_aiff_to_pcm(&path, 0.0, 0.0).unwrap();
        let half = decode_aiff_to_pcm(&path, 0.0, 0.5).unwrap();
        assert!(
            half.samples.len() < full.samples.len(),
            "limited decode should have fewer samples"
        );
        assert!(
            !half.samples.is_empty(),
            "limited decode should still have samples"
        );
    }

    #[test]
    fn decode_aiff_seek_past_end() {
        let path = fixture_path("test.aiff");
        let result = decode_aiff_to_pcm(&path, 999.0, 0.0).unwrap();
        assert!(
            result.samples.is_empty(),
            "seeking past end should produce empty samples"
        );
    }

    #[test]
    fn parse_nonexistent_aiff() {
        let result = parse_aiff("/nonexistent/file.aiff");
        assert!(result.is_err());
    }

    // --- Synthetic AIFF construction test ---

    #[test]
    fn parse_synthetic_aiff() {
        // Build a minimal AIFF in memory and write to a temp file
        let channels: u16 = 1;
        let num_frames: u32 = 4;
        let bits_per_sample: u16 = 16;
        let bytes_per_frame = channels as u32 * (bits_per_sample as u32 / 8);
        let pcm_data_size = num_frames * bytes_per_frame;

        // COMM chunk: 18 bytes
        let mut comm = Vec::new();
        comm.extend_from_slice(b"COMM");
        comm.extend_from_slice(&18u32.to_be_bytes());
        comm.extend_from_slice(&(channels as i16).to_be_bytes());
        comm.extend_from_slice(&num_frames.to_be_bytes());
        comm.extend_from_slice(&(bits_per_sample as i16).to_be_bytes());
        // 44100 Hz in 80-bit extended: exponent=0x400E (16398)
        comm.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        // SSND chunk: 8 (offset+blockSize) + pcm data
        let ssnd_size = 8 + pcm_data_size;
        let mut ssnd = Vec::new();
        ssnd.extend_from_slice(b"SSND");
        ssnd.extend_from_slice(&ssnd_size.to_be_bytes());
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // offset
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // blockSize
        // 4 frames of mono 16-bit: 1000, -1000, 2000, -2000
        ssnd.extend_from_slice(&1000i16.to_be_bytes());
        ssnd.extend_from_slice(&(-1000i16).to_be_bytes());
        ssnd.extend_from_slice(&2000i16.to_be_bytes());
        ssnd.extend_from_slice(&(-2000i16).to_be_bytes());

        // FORM header
        let body_size = comm.len() + ssnd.len();
        let mut form = Vec::new();
        form.extend_from_slice(b"FORM");
        form.extend_from_slice(&(body_size as u32 + 4).to_be_bytes()); // +4 for "AIFF"
        form.extend_from_slice(b"AIFF");
        form.extend_from_slice(&comm);
        form.extend_from_slice(&ssnd);

        // Write to temp file
        let tmp = std::env::temp_dir().join("test_synthetic.aiff");
        std::fs::write(&tmp, &form).unwrap();

        let info = parse_aiff(tmp.to_str().unwrap()).unwrap();
        assert_eq!(info.channels, 1);
        assert_eq!(info.num_frames, 4);
        assert_eq!(info.bits_per_sample, 16);
        assert!((info.sample_rate - 44100.0).abs() < 0.01);
        assert!(!info.is_aifc);

        let decoded = decode_aiff_to_pcm(tmp.to_str().unwrap(), 0.0, 0.0).unwrap();
        assert_eq!(decoded.samples.len(), 4);
        assert_eq!(decoded.samples[0], 1000);
        assert_eq!(decoded.samples[1], -1000);
        assert_eq!(decoded.samples[2], 2000);
        assert_eq!(decoded.samples[3], -2000);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_synthetic_aifc_sowt() {
        // Build a minimal AIFC with "sowt" compression (little-endian PCM)
        let channels: u16 = 1;
        let num_frames: u32 = 2;
        let bits_per_sample: u16 = 16;
        let bytes_per_frame = channels as u32 * (bits_per_sample as u32 / 8);
        let pcm_data_size = num_frames * bytes_per_frame;

        // COMM chunk for AIFC: 18 + 4 (compression type) + 2 (pascal string: len + 1 char + pad)
        let comm_size: u32 = 24;
        let mut comm = Vec::new();
        comm.extend_from_slice(b"COMM");
        comm.extend_from_slice(&comm_size.to_be_bytes());
        comm.extend_from_slice(&(channels as i16).to_be_bytes());
        comm.extend_from_slice(&num_frames.to_be_bytes());
        comm.extend_from_slice(&(bits_per_sample as i16).to_be_bytes());
        comm.extend_from_slice(&[0x40, 0x0D, 0xAC, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        comm.extend_from_slice(b"sowt");
        // Pascal string: length byte + "x" + pad to even
        comm.extend_from_slice(&[1, b'x']);

        // SSND chunk
        let ssnd_size = 8 + pcm_data_size;
        let mut ssnd = Vec::new();
        ssnd.extend_from_slice(b"SSND");
        ssnd.extend_from_slice(&ssnd_size.to_be_bytes());
        ssnd.extend_from_slice(&0u32.to_be_bytes());
        ssnd.extend_from_slice(&0u32.to_be_bytes());
        // Little-endian samples
        ssnd.extend_from_slice(&500i16.to_le_bytes());
        ssnd.extend_from_slice(&(-500i16).to_le_bytes());

        let body_size = comm.len() + ssnd.len();
        let mut form = Vec::new();
        form.extend_from_slice(b"FORM");
        form.extend_from_slice(&(body_size as u32 + 4).to_be_bytes());
        form.extend_from_slice(b"AIFC");
        form.extend_from_slice(&comm);
        form.extend_from_slice(&ssnd);

        let tmp = std::env::temp_dir().join("test_synthetic_sowt.aiff");
        std::fs::write(&tmp, &form).unwrap();

        let info = parse_aiff(tmp.to_str().unwrap()).unwrap();
        assert!(info.is_aifc);
        assert_eq!(info.compression.as_deref(), Some("sowt"));

        let decoded = decode_aiff_to_pcm(tmp.to_str().unwrap(), 0.0, 0.0).unwrap();
        assert_eq!(decoded.samples.len(), 2);
        assert_eq!(decoded.samples[0], 500);
        assert_eq!(decoded.samples[1], -500);

        std::fs::remove_file(&tmp).ok();
    }
}
