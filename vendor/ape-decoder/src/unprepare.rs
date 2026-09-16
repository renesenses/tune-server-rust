/// Channel decorrelation (mid-side reversal) and PCM byte formatting for the
/// Monkey's Audio decoder.
///
/// After the predictor produces decoded sample values (i32 per channel), this
/// module reverses mid-side encoding and writes little-endian PCM bytes.
///
/// Reference: `Prepare.cpp` -- `CPrepare::Unprepare`.
use crate::error::{ApeError, ApeResult};

/// Unprepare decoded sample values into PCM output bytes.
///
/// * `values` -- interleaved decoded samples: for stereo, `[X0, Y0, X1, Y1, ...]`;
///   for mono, `[S0, S1, ...]`.
/// * `channels` -- 1 (mono) or 2 (stereo). Multichannel (>2) is supported for
///   common configurations.
/// * `bits_per_sample` -- 8, 16, 24, or 32.
/// * `output` -- PCM bytes are appended here.
pub fn unprepare(
    values: &[i32],
    channels: u16,
    bits_per_sample: u16,
    output: &mut Vec<u8>,
) -> ApeResult<()> {
    let bytes_per_sample = (bits_per_sample / 8) as usize;
    let num_channels = channels as usize;

    if values.len() % num_channels != 0 {
        return Err(ApeError::DecodingError(
            "sample count not a multiple of channels",
        ));
    }
    let num_blocks = values.len() / num_channels;

    // Reserve space
    output.reserve(values.len() * bytes_per_sample);

    match bits_per_sample {
        32 => unprepare_32(values, channels, num_blocks, output),
        _ if channels > 2 => match bits_per_sample {
            24 => unprepare_multichannel_24(values, channels, num_blocks, output),
            16 => unprepare_multichannel_16(values, channels, num_blocks, output),
            8 => unprepare_multichannel_8(values, channels, num_blocks, output),
            _ => Err(ApeError::DecodingError("unsupported bit depth")),
        },
        _ if channels == 2 => match bits_per_sample {
            16 => unprepare_stereo_16(values, num_blocks, output),
            8 => unprepare_stereo_8(values, num_blocks, output),
            24 => unprepare_stereo_24(values, num_blocks, output),
            _ => Err(ApeError::DecodingError("unsupported bit depth")),
        },
        _ if channels == 1 => match bits_per_sample {
            16 => unprepare_mono_16(values, num_blocks, output),
            8 => unprepare_mono_8(values, num_blocks, output),
            24 => unprepare_mono_24(values, num_blocks, output),
            _ => Err(ApeError::DecodingError("unsupported bit depth")),
        },
        _ => Err(ApeError::DecodingError(
            "unsupported channel/bit-depth combination",
        )),
    }
}

// ===================================================================
// 32-bit
// ===================================================================

fn unprepare_32(
    values: &[i32],
    channels: u16,
    num_blocks: usize,
    output: &mut Vec<u8>,
) -> ApeResult<()> {
    if channels == 2 {
        // Stereo: mid-side decorrelation, write i32 LE. Wrapping to match C++ semantics.
        for block in 0..num_blocks {
            let x = values[block * 2];
            let y = values[block * 2 + 1];
            let first = x.wrapping_sub(y / 2);
            let second = first.wrapping_add(y);

            output.extend_from_slice(&first.to_le_bytes());
            output.extend_from_slice(&second.to_le_bytes());
        }
    } else {
        // All other channel counts: passthrough, write i32 LE
        for &val in values {
            output.extend_from_slice(&val.to_le_bytes());
        }
    }
    Ok(())
}

// ===================================================================
// Stereo 2-channel
// ===================================================================

/// Stereo 16-bit with overflow check.
fn unprepare_stereo_16(values: &[i32], num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for block in 0..num_blocks {
        let x = values[block * 2];
        let y = values[block * 2 + 1];
        let first = x.wrapping_sub(y / 2);
        let second = first.wrapping_add(y);

        // Overflow validation: 16-bit ONLY
        if first < -32768 || first > 32767 || second < -32768 || second > 32767 {
            return Err(ApeError::DecodingError("16-bit sample overflow"));
        }

        output.extend_from_slice(&(first as i16).to_le_bytes());
        output.extend_from_slice(&(second as i16).to_le_bytes());
    }
    Ok(())
}

/// Stereo 8-bit: mid-side with +128 offset, wrapping arithmetic.
fn unprepare_stereo_8(values: &[i32], num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for block in 0..num_blocks {
        let x = values[block * 2];
        let y = values[block * 2 + 1];
        // The +128 bias is integrated into the mid-side formula
        let first: u8 = (x.wrapping_sub(y / 2).wrapping_add(128)) as u8; // wrapping
        let second: u8 = (first as i32 + y) as u8; // wrapping
        output.push(first);
        output.push(second);
    }
    Ok(())
}

/// Stereo 24-bit: special negative encoding.
fn unprepare_stereo_24(values: &[i32], num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for block in 0..num_blocks {
        let x = values[block * 2];
        let y = values[block * 2 + 1];
        let first = x.wrapping_sub(y / 2);
        let second = first.wrapping_add(y);

        write_24bit_special(first, output);
        write_24bit_special(second, output);
    }
    Ok(())
}

/// 24-bit special negative encoding for stereo/mono:
/// if value < 0: temp = (value + 0x800000) as u32 | 0x800000
/// else: temp = value as u32
/// Then write 3 bytes LE.
#[inline(always)]
fn write_24bit_special(value: i32, output: &mut Vec<u8>) {
    let temp: u32 = if value < 0 {
        ((value + 0x80_0000) as u32) | 0x80_0000
    } else {
        value as u32
    };
    output.push((temp & 0xFF) as u8);
    output.push(((temp >> 8) & 0xFF) as u8);
    output.push(((temp >> 16) & 0xFF) as u8);
}

/// 24-bit simple encoding for multichannel: straight u32 cast, extract low 3 bytes.
#[inline(always)]
fn write_24bit_simple(value: i32, output: &mut Vec<u8>) {
    let temp = value as u32;
    output.push((temp & 0xFF) as u8);
    output.push(((temp >> 8) & 0xFF) as u8);
    output.push(((temp >> 16) & 0xFF) as u8);
}

// ===================================================================
// Mono 1-channel
// ===================================================================

fn unprepare_mono_16(values: &[i32], _num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for &val in values {
        output.extend_from_slice(&(val as i16).to_le_bytes());
    }
    Ok(())
}

fn unprepare_mono_8(values: &[i32], _num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for &val in values {
        output.push((val + 128) as u8);
    }
    Ok(())
}

fn unprepare_mono_24(values: &[i32], _num_blocks: usize, output: &mut Vec<u8>) -> ApeResult<()> {
    for &val in values {
        write_24bit_special(val, output);
    }
    Ok(())
}

// ===================================================================
// Multichannel (>2 channels)
// ===================================================================

/// Apply mid-side decorrelation to a pair of values.
#[inline(always)]
fn mid_side(x: i32, y: i32) -> (i32, i32) {
    let first = x - (y / 2);
    let second = first + y;
    (first, second)
}

/// Multichannel 16-bit.
fn unprepare_multichannel_16(
    values: &[i32],
    channels: u16,
    num_blocks: usize,
    output: &mut Vec<u8>,
) -> ApeResult<()> {
    let nc = channels as usize;

    for block in 0..num_blocks {
        let base = block * nc;
        let mut out_samples = vec![0i32; nc];

        apply_multichannel_decorrelation(&values[base..base + nc], channels, &mut out_samples);

        // Write 16-bit LE with overflow check for mid-side pairs
        for ch in 0..nc {
            let val = out_samples[ch];
            if val < -32768 || val > 32767 {
                return Err(ApeError::DecodingError("16-bit sample overflow"));
            }
            output.extend_from_slice(&(val as i16).to_le_bytes());
        }
    }
    Ok(())
}

/// Multichannel 24-bit.
fn unprepare_multichannel_24(
    values: &[i32],
    channels: u16,
    num_blocks: usize,
    output: &mut Vec<u8>,
) -> ApeResult<()> {
    let nc = channels as usize;

    for block in 0..num_blocks {
        let base = block * nc;
        let mut out_samples = vec![0i32; nc];

        apply_multichannel_decorrelation(&values[base..base + nc], channels, &mut out_samples);

        // Multichannel 24-bit uses simple encoding (u32 cast, not special negative)
        for ch in 0..nc {
            write_24bit_simple(out_samples[ch], output);
        }
    }
    Ok(())
}

/// Multichannel 8-bit: passthrough all channels, value + 128.
fn unprepare_multichannel_8(
    values: &[i32],
    channels: u16,
    num_blocks: usize,
    output: &mut Vec<u8>,
) -> ApeResult<()> {
    let nc = channels as usize;

    for block in 0..num_blocks {
        let base = block * nc;
        for ch in 0..nc {
            output.push((values[base + ch] + 128) as u8);
        }
    }
    Ok(())
}

/// Apply multichannel decorrelation pattern based on channel count.
///
/// * 4 channels: two mid-side pairs (0,1) and (2,3).
/// * 6+ channels: (0,1) mid-side, (2,3) passthrough, (4,5) mid-side,
///   (6,7) mid-side if 8+ channels, remaining passthrough.
/// * 3 or 5 channels: all passthrough.
fn apply_multichannel_decorrelation(input: &[i32], channels: u16, out: &mut [i32]) {
    let nc = channels as usize;

    match nc {
        4 => {
            let (f0, s0) = mid_side(input[0], input[1]);
            let (f1, s1) = mid_side(input[2], input[3]);
            out[0] = f0;
            out[1] = s0;
            out[2] = f1;
            out[3] = s1;
        }
        n if n >= 6 => {
            // Channels 0,1: mid-side
            let (f0, s0) = mid_side(input[0], input[1]);
            out[0] = f0;
            out[1] = s0;

            // Channels 2,3: passthrough
            out[2] = input[2];
            out[3] = input[3];

            // Channels 4,5: mid-side
            let (f2, s2) = mid_side(input[4], input[5]);
            out[4] = f2;
            out[5] = s2;

            if n >= 8 {
                // Channels 6,7: mid-side (rear pair)
                let (f3, s3) = mid_side(input[6], input[7]);
                out[6] = f3;
                out[7] = s3;

                // Remaining channels (8+): passthrough
                for ch in 8..n {
                    out[ch] = input[ch];
                }
            } else {
                // 6 or 7 channels: remaining passthrough
                // For 7 channels, channel 6 is not written by any mid-side
                // block (matching SDK behavior -- see doc note).
                let start = if n == 7 { 7 } else { 6 };
                for ch in start..n {
                    out[ch] = input[ch];
                }
                // For exactly 6 channels, the loop 6..6 is empty.
                // For exactly 7 channels, the loop 7..7 is empty, leaving
                // channel 6 at its default (0). This matches SDK behavior.
            }
        }
        _ => {
            // 3 or 5 channels: passthrough all
            out[..nc].copy_from_slice(&input[..nc]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_16bit_basic() {
        // X=100, Y=20 -> first = 100 - 10 = 90, second = 90 + 20 = 110
        let values = vec![100i32, 20i32];
        let mut out = Vec::new();
        unprepare(&values, 2, 16, &mut out).unwrap();
        assert_eq!(out.len(), 4);
        let first = i16::from_le_bytes([out[0], out[1]]);
        let second = i16::from_le_bytes([out[2], out[3]]);
        assert_eq!(first, 90);
        assert_eq!(second, 110);
    }

    #[test]
    fn stereo_16bit_overflow() {
        // Values that exceed i16 range after mid-side
        let values = vec![40000i32, 0i32];
        let mut out = Vec::new();
        assert!(unprepare(&values, 2, 16, &mut out).is_err());
    }

    #[test]
    fn mono_8bit() {
        let values = vec![0i32, -128i32, 127i32];
        let mut out = Vec::new();
        unprepare(&values, 1, 8, &mut out).unwrap();
        assert_eq!(out, vec![128u8, 0u8, 255u8]);
    }

    #[test]
    fn stereo_8bit_wrapping() {
        // X=0, Y=0 -> first = (0 - 0 + 128) as u8 = 128, second = (128 + 0) as u8 = 128
        let values = vec![0i32, 0i32];
        let mut out = Vec::new();
        unprepare(&values, 2, 8, &mut out).unwrap();
        assert_eq!(out, vec![128u8, 128u8]);
    }

    #[test]
    fn stereo_32bit() {
        let values = vec![1000i32, 200i32];
        let mut out = Vec::new();
        unprepare(&values, 2, 32, &mut out).unwrap();
        assert_eq!(out.len(), 8);
        let first = i32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        let second = i32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!(first, 1000 - 100);
        assert_eq!(second, 900 + 200);
    }

    #[test]
    fn mono_24bit_negative() {
        let values = vec![-1i32];
        let mut out = Vec::new();
        unprepare(&values, 1, 24, &mut out).unwrap();
        // -1 + 0x800000 = 0x7FFFFF, | 0x800000 = 0xFFFFFF
        assert_eq!(out, vec![0xFF, 0xFF, 0xFF]);
    }
}
