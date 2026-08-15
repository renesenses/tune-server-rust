/// Largest `data` chunk size we advertise when the real length is unknown.
///
/// Not `0x7FFF_FFFF`: the RIFF size is `data_size + 36`, so the obvious
/// `i32::MAX` makes the RIFF field `0x8000_0023` — **negative** for any parser
/// that reads it into a signed 32-bit integer. Backing off by 36 bytes puts the
/// RIFF size at exactly `i32::MAX` and keeps both fields positive (#1689). The
/// 36 bytes lost out of 2 GiB are ~0.2 ms of CD audio.
const UNKNOWN_DATA_SIZE: u32 = 0x7FFF_FFFF - 36;

/// Total response length announced for a live stream served with the *file*
/// contract — `Content-Length` + `Range` — instead of a chunked, length-less
/// body.
///
/// Some renderers refuse chunked transfer outright and will not start without a
/// length (darTZeel LHC-208, session support du 01/08 ; #1689). A live stream
/// has no length, so we announce one: as large as possible while staying
/// positive in a signed 32-bit integer, header included. `i32::MAX` is ~2 GiB
/// ≈ 3 h 22 of 44.1/16/2 — longer than any listening session, and the stream
/// simply ends early if the station stops, exactly as it does today.
pub const LIVE_BOUNDED_TOTAL_LEN: u64 = i32::MAX as u64;

/// `data` chunk size matching [`LIVE_BOUNDED_TOTAL_LEN`], header deducted.
pub const LIVE_BOUNDED_DATA_SIZE: u32 = (LIVE_BOUNDED_TOTAL_LEN - 44) as u32;

/// Build the 44-byte WAV header for a live stream served as a bounded file.
/// Sizes are finite and positive as `i32`, so a renderer that parses the header
/// to plan a ranged fetch can do so (see [`LIVE_BOUNDED_TOTAL_LEN`]).
pub fn build_wav_header_bounded_live(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    build_wav_header_with_data_size(channels, sample_rate, bit_depth, LIVE_BOUNDED_DATA_SIZE)
}

/// Build a 44-byte WAV header. When `duration_ms` is provided, the header
/// contains the correct data size so DLNA renderers don't need to probe
/// the stream end. Falls back to [`UNKNOWN_DATA_SIZE`] for unknown-length
/// streams.
pub fn build_wav_header(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    build_wav_header_with_duration(channels, sample_rate, bit_depth, None)
}

pub fn build_wav_header_with_duration(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    duration_ms: Option<u64>,
) -> [u8; 44] {
    let data_size: u32 = if let Some(dur) = duration_ms {
        // Saturant : une durée aberrante (tag corrompu) débordait le produit et
        // faisait paniquer le constructeur d'en-tête en debug.
        let bytes = dur
            .saturating_mul(sample_rate as u64)
            .saturating_mul(channels as u64)
            .saturating_mul(bit_depth as u64 / 8)
            / 1000;
        bytes.min(UNKNOWN_DATA_SIZE as u64) as u32
    } else {
        UNKNOWN_DATA_SIZE
    };
    build_wav_header_with_data_size(channels, sample_rate, bit_depth, data_size)
}

/// Build a 44-byte WAV header for an INFINITE live stream (internet radio).
///
/// Uses the "indeterminate length" convention (`RIFF` and `data` chunk sizes
/// both set to `0xFFFF_FFFF`) instead of a finite size. An FFmpeg/libavformat
/// (`Lavf`) DLNA renderer treats `0xFFFF_FFFF` as an unbounded stream and keeps
/// reading until the connection closes, whereas the previous finite
/// `0x7FFF_FFFF` (~2 GiB) size makes it treat the transcoded radio as a bounded
/// PCM file: it fills its ~64 MiB read-ahead cache and then stops/reconnects
/// after ~6 minutes (FIP cutoff, .15 zone_id=10, `Lavf/58.45.100`).
pub fn build_wav_header_streaming(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    // Both the RIFF and the data chunk sizes are set to 0xFFFF_FFFF (the
    // canonical "unknown/streaming length" marker). build_wav_header_with_data_size
    // would wrap the RIFF size to `data_size + 36`, so patch it to 0xFFFF_FFFF here.
    let mut header = build_wav_header_with_data_size(channels, sample_rate, bit_depth, 0xFFFF_FFFF);
    header[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    header
}

/// Build a 44-byte WAV header with an exact `data` chunk size, for complete
/// (non-streaming) WAV files where the full PCM length is known upfront.
pub fn build_wav_header_with_data_size(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    data_size: u32,
) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * (bit_depth as u32 / 8);
    let block_align = channels * (bit_depth / 8);
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

    #[test]
    fn wav_header_mono() {
        let h = build_wav_header(1, 44100, 16);
        let channels = u16::from_le_bytes([h[22], h[23]]);
        assert_eq!(channels, 1);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 88200); // 44100 * 1 * 2
        let block_align = u16::from_le_bytes([h[32], h[33]]);
        assert_eq!(block_align, 2); // 1 * 2
    }

    #[test]
    fn wav_header_24bit() {
        let h = build_wav_header(2, 96000, 24);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 576000); // 96000 * 2 * 3
        let block_align = u16::from_le_bytes([h[32], h[33]]);
        assert_eq!(block_align, 6); // 2 * 3
        let bit_depth = u16::from_le_bytes([h[34], h[35]]);
        assert_eq!(bit_depth, 24);
    }

    #[test]
    fn wav_header_hires() {
        let h = build_wav_header(2, 192000, 24);
        let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
        assert_eq!(sample_rate, 192000);
    }

    #[test]
    fn wav_header_pcm_format() {
        let h = build_wav_header(2, 44100, 16);
        let format = u16::from_le_bytes([h[20], h[21]]);
        assert_eq!(format, 1); // PCM
    }

    #[test]
    fn wav_header_fmt_chunk_size() {
        let h = build_wav_header(2, 44100, 16);
        let chunk_size = u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
        assert_eq!(chunk_size, 16);
    }

    #[test]
    fn wav_header_data_size() {
        let h = build_wav_header(2, 44100, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, UNKNOWN_DATA_SIZE);
    }

    #[test]
    fn unknown_length_header_stays_positive_as_a_signed_int() {
        // Un lecteur qui lit ces deux champs dans un entier 32 bits SIGNÉ doit
        // y voir une taille positive. Avec 0x7FFF_FFFF, le champ RIFF valait
        // data + 36 = 0x8000_0023, soit -2147483613 : « plus rien à lire »
        // avant même d'avoir commencé (#1689).
        let h = build_wav_header(2, 44100, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert!(data_size as i32 > 0, "data size must be a positive i32");
        assert!(riff_size as i32 > 0, "RIFF size must be a positive i32");
        assert_eq!(riff_size, i32::MAX as u32);
    }

    #[test]
    fn bounded_live_header_and_content_length_agree_and_stay_positive() {
        // Le lecteur lit Content-Length ET les tailles de l'en-tête. Les trois
        // doivent rester positives en 32 bits signés, et l'en-tête doit décrire
        // exactement le nombre d'octets annoncé par Content-Length (#1689).
        let h = build_wav_header_bounded_live(2, 44100, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(data_size, LIVE_BOUNDED_DATA_SIZE);
        assert_eq!(
            data_size as u64 + 44,
            LIVE_BOUNDED_TOTAL_LEN,
            "en-tête + data doivent faire exactement le Content-Length annoncé"
        );
        assert!(LIVE_BOUNDED_TOTAL_LEN <= i32::MAX as u64);
        assert!(data_size as i32 > 0);
        assert!(riff_size as i32 > 0);
        // Le format décrit toujours le vrai flux.
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 44100);
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2);
    }

    #[test]
    fn bounded_live_covers_a_long_listening_session() {
        // ~3 h 22 en 44,1/16/2 : plus long que n'importe quelle écoute.
        let secs = LIVE_BOUNDED_DATA_SIZE as u64 / (44100 * 2 * 2);
        assert!(secs > 3 * 3600, "seulement {secs} s de direct annoncées");
    }

    #[test]
    fn wav_header_with_duration_is_clamped_below_the_signed_limit() {
        // Même garantie quand une durée absurde est fournie : le plafond doit
        // laisser la place aux 36 octets d'en-tête.
        let h = build_wav_header_with_duration(2, 192_000, 24, Some(u64::MAX / 1_000_000));
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert!(riff_size as i32 > 0, "RIFF size must be a positive i32");
    }

    #[test]
    fn wav_header_with_known_duration() {
        // 3 minutes of 44100/16/2 = 180s * 176400 bytes/s = 31752000 bytes
        let h = build_wav_header_with_duration(2, 44100, 16, Some(180_000));
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 180 * 44100 * 2 * 2);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(riff_size, data_size + 36);
    }

    #[test]
    fn wav_header_without_duration_uses_max() {
        let h = build_wav_header_with_duration(2, 44100, 16, None);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, UNKNOWN_DATA_SIZE);
    }

    #[test]
    fn wav_header_streaming_uses_indeterminate_length() {
        // Live radio: both RIFF and data chunk sizes must be 0xFFFF_FFFF so a
        // Lavf renderer treats the stream as unbounded and reads until close,
        // instead of stopping at the finite 0x7FFF_FFFF (~2 GiB) size.
        let h = build_wav_header_streaming(2, 48000, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(data_size, 0xFFFF_FFFF);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(riff_size, 0xFFFF_FFFF);
        // Format fields still reflect the true stream: 48000/16/2.
        let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
        assert_eq!(sample_rate, 48000);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        assert_eq!(byte_rate, 48000 * 2 * 2);
    }

    #[test]
    fn wav_header_duration_hires() {
        // 4:16.487 of 96000/24/2
        let h = build_wav_header_with_duration(2, 96000, 24, Some(256_487));
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        let expected = 256_487u64 * 96000 * 2 * 3 / 1000;
        assert_eq!(data_size, expected as u32);
    }
}
