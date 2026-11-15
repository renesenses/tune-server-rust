//! Tune (#8489): capture from devices that only offer packed 24-bit samples
//! (`S24_3LE` / `S24_3BE`, three bytes per sample), such as the miniDSP UMIK-1
//! and UMIK-2 measurement microphones.
//!
//! Upstream CPAL lists no 3-byte format, so such a device has no supported
//! configuration at all and `default_input_config` fails with
//! `StreamTypeNotSupported`. For CAPTURE only, the ALSA host now opens the
//! packed format and hands the user `SampleFormat::I24` (a 24-bit value in an
//! `i32`), unpacking each period here. Playback is unchanged.
//!
//! This file is self-contained (no `crate::`, no `alsa`): Tune compiles it
//! as is under `#[path]` in `tune-core/tests/alsa_s24_3_8489.rs`.

/// Byte order of a packed 24-bit sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packed24 {
    /// `SND_PCM_FORMAT_S24_3LE`.
    Le,
    /// `SND_PCM_FORMAT_S24_3BE`.
    Be,
}

/// Bytes per packed sample.
pub const PACKED24_BYTES: usize = 3;

impl Packed24 {
    /// The packed layout to open for a capture stream asked in `I24`, or
    /// `None` to keep CPAL's behaviour.
    ///
    /// - only for capture;
    /// - only when the hardware refuses the 4-byte `S24` formats (a device
    ///   offering both keeps the 4-byte one, as before);
    /// - native endianness first, as `sample_format_to_alsa_format` does.
    pub fn for_capture(
        capture: bool,
        has_s24_4_bytes: bool,
        s24_3le: bool,
        s24_3be: bool,
    ) -> Option<Self> {
        if !capture || has_s24_4_bytes {
            return None;
        }
        let (native, native_ok, opposite, opposite_ok) = if cfg!(target_endian = "little") {
            (Packed24::Le, s24_3le, Packed24::Be, s24_3be)
        } else {
            (Packed24::Be, s24_3be, Packed24::Le, s24_3le)
        };
        if native_ok {
            Some(native)
        } else if opposite_ok {
            Some(opposite)
        } else {
            None
        }
    }

    /// One packed sample (3 bytes) to its signed value, sign-extended.
    #[inline]
    pub fn sample(self, b: [u8; 3]) -> i32 {
        let (lo, mid, hi) = match self {
            Packed24::Le => (b[0], b[1], b[2]),
            Packed24::Be => (b[2], b[1], b[0]),
        };
        // Put the 24 bits at the top of an i32, then shift back: the
        // arithmetic shift copies the sign bit.
        i32::from_le_bytes([0, lo, mid, hi]) >> 8
    }
}

/// Unpacks `src` (whole packed samples) into `dst` as native-endian `i32`,
/// the layout CPAL's `I24` data uses. Returns the number of samples written.
///
/// `dst` must hold `4 * src.len() / 3` bytes; extra bytes in either buffer
/// are left alone.
pub fn unpack_s24_3(layout: Packed24, src: &[u8], dst: &mut [u8]) -> usize {
    let n = (src.len() / PACKED24_BYTES).min(dst.len() / 4);
    for (s, d) in src
        .chunks_exact(PACKED24_BYTES)
        .zip(dst.chunks_exact_mut(4))
        .take(n)
    {
        let v = layout.sample([s[0], s[1], s[2]]);
        d.copy_from_slice(&v.to_ne_bytes());
    }
    n
}
