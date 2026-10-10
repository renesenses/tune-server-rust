//! Tune (#5643): pure helpers of the ASIO native DSD output path.
//!
//! Self-contained on purpose: no `crate::` path, no `asio_sys` type. The
//! ASIO host (`stream.rs`, Windows only) uses it, and Tune compiles
//! this very file on Linux (`tune-core/tests/asio_dsd_5643.rs`, via `#[path]`)
//! to test it without the ASIO SDK.
//!
//! Conventions:
//! - the CPAL side is `SampleFormat::DsdU8`, interleaved one byte per channel
//!   per frame, first DSD bit in the most significant bit (same layout as
//!   ALSA `DSD_U8`);
//! - an ASIO DSD buffer size is counted in DSD samples, i.e. bits, per
//!   channel (asio.h: "we always deal with a multiple of 8 samples"). CPAL
//!   asks for a size in BYTES per channel: one byte is 8 ASIO samples.

#![allow(dead_code)]

/// `ASIOSTDSDInt8LSB1`: 8 DSD samples per byte, first sample in the LSB.
pub const ASIO_ST_DSD_INT8_LSB1: i32 = 32;
/// `ASIOSTDSDInt8MSB1`: 8 DSD samples per byte, first sample in the MSB.
pub const ASIO_ST_DSD_INT8_MSB1: i32 = 33;
/// `ASIOSTDSDInt8NER8`: 8-bit DSD words, one sample per byte.
pub const ASIO_ST_DSD_INT8_NER8: i32 = 40;

/// DSD rates accepted for native output: DSD64, DSD128, DSD256.
pub const DSD_RATES: [u32; 3] = [2_822_400, 5_644_800, 11_289_600];

/// DSD idle pattern, MSB first (the `DsdU8` convention).
pub const DSD_SILENCE_MSB_FIRST: u8 = 0x69;

/// Byte layout of an ASIO DSD channel buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DsdLayout {
    /// `ASIOSTDSDInt8MSB1`: bytes copied as is.
    Msb1,
    /// `ASIOSTDSDInt8LSB1`: bit order reversed in every byte.
    Lsb1,
    /// `ASIOSTDSDInt8NER8`: 8-bit DSD words ("DSD-Wide"), NOT a packed
    /// 1-bit stream. Recognised so that it is never mistaken for PCM or for
    /// packed DSD, but no `DsdU8` stream is opened on it.
    Ner8,
}

impl DsdLayout {
    /// Reads an `ASIOSampleType` value. `None` for every PCM type.
    pub const fn from_asio_type(code: i32) -> Option<DsdLayout> {
        match code {
            ASIO_ST_DSD_INT8_MSB1 => Some(DsdLayout::Msb1),
            ASIO_ST_DSD_INT8_LSB1 => Some(DsdLayout::Lsb1),
            ASIO_ST_DSD_INT8_NER8 => Some(DsdLayout::Ner8),
            _ => None,
        }
    }

    /// Packed 1-bit layouts, the only ones a `DsdU8` stream can feed.
    pub const fn carries_dsd_u8(self) -> bool {
        matches!(self, DsdLayout::Msb1 | DsdLayout::Lsb1)
    }

    /// One `DsdU8` byte as the driver expects it in its buffer.
    #[inline]
    pub const fn encode(self, msb_first: u8) -> u8 {
        match self {
            DsdLayout::Lsb1 => msb_first.reverse_bits(),
            DsdLayout::Msb1 | DsdLayout::Ner8 => msb_first,
        }
    }

    /// The idle pattern in this layout.
    pub const fn silence(self) -> u8 {
        self.encode(DSD_SILENCE_MSB_FIRST)
    }
}

/// Is `rate` a DSD rate this path opens?
pub fn is_dsd_rate(rate: u32) -> bool {
    DSD_RATES.contains(&rate)
}

/// ASIO buffer size (DSD samples per channel) for `bytes` per channel.
/// `None` for zero or on overflow of the driver's `long`.
pub fn asio_samples_for_bytes(bytes: u32) -> Option<i32> {
    if bytes == 0 {
        return None;
    }
    let samples = bytes.checked_mul(8)?;
    i32::try_from(samples).ok()
}

/// Bytes per channel for an ASIO buffer size in DSD samples. `None` when
/// the size is not positive or not a whole number of bytes.
pub fn bytes_for_asio_samples(samples: i32) -> Option<usize> {
    if samples <= 0 || samples % 8 != 0 {
        return None;
    }
    Some(samples as usize / 8)
}

/// Writes channel `channel` of the interleaved `DsdU8` buffer into the ASIO
/// channel buffer `dst`, converting to `layout`. Copies
/// `min(dst.len(), frames in interleaved)` bytes; returns that count.
pub fn write_channel(
    interleaved: &[u8],
    n_channels: usize,
    channel: usize,
    layout: DsdLayout,
    dst: &mut [u8],
) -> usize {
    if n_channels == 0 || channel >= n_channels {
        return 0;
    }
    let mut written = 0;
    for (out, frame) in dst.iter_mut().zip(interleaved.chunks_exact(n_channels)) {
        *out = layout.encode(frame[channel]);
        written += 1;
    }
    written
}
