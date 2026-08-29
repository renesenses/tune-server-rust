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
/// Nombre d'octets suffisant pour reconnaître un format ici : `detect_and_parse`
/// n'inspecte jamais au-delà.
pub(super) const ENTETE_DETECTION: usize = 92;

/// Cet en-tête est-il celui d'un WAV, seul format que la lecture directe OAAT
/// sait réellement jouer ?
///
/// Le FLAC n'a pas de chemin de lecture directe ici (il repart sur le flux HTTP
/// que l'orchestrateur sert déjà), le DSD doit être converti en PCM, et les
/// formats compressés ne sont pas parsés du tout. Tout sauf le WAV finit donc
/// en HTTP — autant s'en apercevoir sur 12 octets plutôt qu'après avoir chargé
/// le fichier entier.
///
/// Mesuré sur .42, DSD128 de 868 Mo : quinze secondes de silence entre
/// `play_media` et la bascule, et un pic mémoire de presque un gigaoctet sur un
/// mini PC. L'utilisateur monte le volume, n'entend rien, met en pause — et la
/// conversion démarre après qu'il a abandonné.
pub(super) fn entete_est_wav(entete: &[u8]) -> bool {
    entete.len() >= 12 && &entete[..4] == b"RIFF" && &entete[8..12] == b"WAVE"
}

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

    // A progressive transcode writes its header before the length is known and
    // fills the data chunk with a sentinel: 0x7FFF_FFFF for a bounded stream,
    // 0xFFFF_FFFF for an open-ended one (see `audio::wav`). Those are not
    // sizes, and converting them yields a plausible-looking duration that is
    // pure fiction — 3 h 23 at 44.1/16/2, but only 31 minutes at 192/24/2,
    // which no duration threshold can tell from a real long track. Reporting 0
    // (unknown) is the honest answer, and callers already handle it.
    const DATA_SIZE_UNKNOWN: [u64; 2] = [0x7FFF_FFFF, 0xFFFF_FFFF];
    let bytes_per_frame = (bits_per_sample as u64 / 8) * channels as u64;
    let duration_ms = if bytes_per_frame > 0
        && sample_rate > 0
        && data_size > 0
        && !DATA_SIZE_UNKNOWN.contains(&data_size)
    {
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

/// Payload restant a l'EOF, avec le drapeau de fin porte par le dernier
/// payload reel. Un flux vide produit tout de meme un unique paquet LAST vide.
#[derive(Debug)]
pub(super) struct PayloadFinFlux {
    pub bytes: Vec<u8>,
    pub dernier: bool,
}

/// Decoupe le tampon final sans perdre d'octet.
///
/// `taille_trame = Some(n)` impose l'alignement PCM : un residu incomplet est
/// une erreur de contrat, pas un octet a jeter. `None` conserve chaque octet
/// d'un format compresse comme FLAC.
pub(super) fn extraire_payloads_fin_flux(
    buf: &mut Vec<u8>,
    taille_paquet: usize,
    taille_trame: Option<usize>,
) -> Result<Vec<PayloadFinFlux>, String> {
    if taille_paquet == 0 {
        return Err("taille de paquet finale nulle".into());
    }
    if let Some(taille_trame) = taille_trame {
        if taille_trame == 0 {
            return Err("taille de trame finale nulle".into());
        }
        if !taille_paquet.is_multiple_of(taille_trame) {
            return Err(format!(
                "taille de paquet {taille_paquet} non alignee sur une trame de {taille_trame} octets"
            ));
        }
        if !buf.len().is_multiple_of(taille_trame) {
            return Err(format!(
                "residu PCM de {} octets non aligne sur une trame de {taille_trame} octets",
                buf.len()
            ));
        }
    }

    if buf.is_empty() {
        return Ok(vec![PayloadFinFlux {
            bytes: Vec::new(),
            dernier: true,
        }]);
    }

    let bytes = std::mem::take(buf);
    let nombre = bytes.len().div_ceil(taille_paquet);
    Ok(bytes
        .chunks(taille_paquet)
        .enumerate()
        .map(|(index, chunk)| PayloadFinFlux {
            bytes: chunk.to_vec(),
            dernier: index + 1 == nombre,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier_payloads_finaux(taille: usize, taille_paquet: usize, taille_trame: Option<usize>) {
        let attendu: Vec<u8> = (0..taille).map(|i| i as u8).collect();
        let mut buf = attendu.clone();
        let payloads = extraire_payloads_fin_flux(&mut buf, taille_paquet, taille_trame).unwrap();

        assert!(buf.is_empty());
        assert_eq!(payloads.iter().filter(|p| p.dernier).count(), 1);
        assert!(payloads.last().unwrap().dernier);
        assert!(payloads.iter().all(|p| p.bytes.len() <= taille_paquet));
        if let Some(taille_trame) = taille_trame {
            assert!(
                payloads
                    .iter()
                    .all(|p| p.bytes.len().is_multiple_of(taille_trame))
            );
        }
        let obtenu: Vec<u8> = payloads.into_iter().flat_map(|p| p.bytes).collect();
        assert_eq!(obtenu, attendu);
    }

    #[test]
    fn fin_pcm_conserve_toutes_les_trames_mono_stereo_et_24_bits() {
        for taille_trame in [2, 4, 6] {
            let taille_paquet = taille_trame * 8;
            for taille in [
                0,
                taille_trame,
                taille_paquet - taille_trame,
                taille_paquet,
                taille_paquet + taille_trame,
            ] {
                verifier_payloads_finaux(taille, taille_paquet, Some(taille_trame));
            }
        }
    }

    #[test]
    fn fin_flac_conserve_tous_les_octets_sans_alignement_pcm() {
        let taille_paquet = 16;
        for taille in [0, 1, taille_paquet - 1, taille_paquet, taille_paquet + 1] {
            verifier_payloads_finaux(taille, taille_paquet, None);
        }
    }

    #[test]
    fn fin_pcm_refuse_un_octet_incomplet_sans_le_jeter() {
        let mut buf = vec![1, 2, 3];
        let erreur = extraire_payloads_fin_flux(&mut buf, 16, Some(4)).unwrap_err();
        assert!(erreur.contains("non aligne"));
        assert_eq!(buf, vec![1, 2, 3]);
    }

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

/// Nanoseconds since UNIX epoch (controller clock domain for OAAT PTS).
pub(crate) fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Base de temps du cadencement OAAT.
///
/// PCM et FLAC avancent avec un nombre de samples. Seul un flux dont le débit
/// en octets est réellement constant — le porteur DSD — peut être cadencé
/// depuis les octets envoyés. Le type rend ce choix visible à chaque appel et
/// empêche de remettre FLAC dans la branche octets par commodité (#2214).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BaseDeTempsOaat {
    Samples,
    OctetsADebitConstant,
}

pub(super) fn duree_audio_envoyee(
    base: BaseDeTempsOaat,
    sample_offset: u64,
    byte_offset: u64,
    sample_rate: u32,
    bytes_per_frame: usize,
) -> std::time::Duration {
    let (unites, unites_par_seconde) = match base {
        BaseDeTempsOaat::Samples => (sample_offset as u128, sample_rate as u128),
        BaseDeTempsOaat::OctetsADebitConstant => (
            byte_offset as u128,
            sample_rate as u128 * bytes_per_frame as u128,
        ),
    };
    if unites_par_seconde == 0 {
        return std::time::Duration::ZERO;
    }
    let nanos = unites
        .saturating_mul(1_000_000_000)
        .checked_div(unites_par_seconde)
        .unwrap_or(0)
        .min(u64::MAX as u128) as u64;
    std::time::Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests_cadencement_oaat {
    use super::{BaseDeTempsOaat, duree_audio_envoyee};

    #[test]
    fn la_duree_flac_depend_des_samples_pas_de_la_taille_compressee() {
        let depuis_petit_corps =
            duree_audio_envoyee(BaseDeTempsOaat::Samples, 44_100, 4_096, 44_100, 4);
        let depuis_gros_corps =
            duree_audio_envoyee(BaseDeTempsOaat::Samples, 44_100, 4_000_000, 44_100, 4);

        assert_eq!(depuis_petit_corps, std::time::Duration::from_secs(1));
        assert_eq!(depuis_gros_corps, depuis_petit_corps);
    }

    #[test]
    fn le_porteur_dsd_conserve_son_horloge_a_debit_octets_constant() {
        let duree = duree_audio_envoyee(
            BaseDeTempsOaat::OctetsADebitConstant,
            99,
            1_152_000,
            192_000,
            6,
        );
        assert_eq!(duree, std::time::Duration::from_secs(1));
    }

    #[test]
    fn une_cadence_inconnue_ne_divise_jamais_par_zero() {
        assert_eq!(
            duree_audio_envoyee(BaseDeTempsOaat::Samples, 1, 0, 0, 0),
            std::time::Duration::ZERO
        );
    }
}

/// Process-wide OAAT clock sync responder port (one clock master identity
/// for the whole server). Bound once on first use; answers endpoint-initiated
/// exchanges (OAAT RFC §6.2) so endpoints can PTS-schedule playback against
/// our clock. Returns 0 if binding failed (announced as "no responder").
pub(crate) fn oaat_clock_port() -> u16 {
    use std::sync::OnceLock;
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        let sock = std::net::UdpSocket::bind(("0.0.0.0", oaat_core::DEFAULT_CLOCK_PORT))
            .or_else(|_| std::net::UdpSocket::bind(("0.0.0.0", 0)));
        match sock {
            Ok(s) => {
                if s.set_nonblocking(true).is_err() {
                    return 0;
                }
                let port = s.local_addr().map(|a| a.port()).unwrap_or(0);
                tokio::spawn(async move {
                    let Ok(sock) = tokio::net::UdpSocket::from_std(s) else {
                        return;
                    };
                    clock_responder_loop(sock).await;
                });
                tracing::info!(port, "oaat: clock sync responder listening");
                port
            }
            Err(e) => {
                tracing::warn!(error = %e, "oaat: clock responder unavailable, endpoints cannot sync");
                0
            }
        }
    })
}

/// Answer OAAT clock sync requests: stamp t2/t3 and echo t1 back.
async fn clock_responder_loop(socket: tokio::net::UdpSocket) {
    use oaat_core::wire::{ClockSyncPacket, ClockSyncType};
    let mut buf = [0u8; ClockSyncPacket::SIZE];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "oaat: clock responder recv error");
                break;
            }
        };
        if n < ClockSyncPacket::SIZE {
            continue;
        }
        let Ok(pkt) = ClockSyncPacket::decode(&buf) else {
            continue;
        };
        if pkt.kind != ClockSyncType::Request {
            continue;
        }
        let t2 = now_ns();
        let t3 = now_ns();
        let response = ClockSyncPacket {
            version: 1,
            kind: ClockSyncType::Response,
            sequence: pkt.sequence,
            t1: pkt.t1,
            t2,
            t3,
        };
        let mut resp = [0u8; ClockSyncPacket::SIZE];
        response.encode(&mut resp);
        let _ = socket.send_to(&resp, peer).await;
    }
}

/// Does a stream body error land at the natural end of the track, rather than
/// mid-track?
///
/// The server advertises `Content-Length` for a progressive WAV transcode from
/// the **library** duration (`StreamInfo::wav_content_length`), while the
/// decoder emits the file's exact sample count. The two differ — at 88.2 kHz /
/// 24-bit / stereo, one millisecond is already ~530 bytes — so the body ends
/// short of its declared length and reqwest reports `error decoding response
/// body` instead of a clean EOF, at the precise moment the track ends.
///
/// Xavier Joly, 7 Aug 2026: track started 16:31:04, duration 177.9 s, natural
/// end 16:34:01.9 — body error logged at 16:34:01. Read as a mid-stream
/// failure, the loop attempted a Range resume that could not succeed, gave up,
/// and left by a path that skips the gapless transition entirely: 83 seconds of
/// silence before the next track came back on a cold session.
///
/// `tolerance_ms` bounds what we accept as "the end". A genuine failure inside
/// that window costs at most that much audio; getting it wrong the other way
/// costs the whole track chain.
pub(super) fn body_error_is_track_end(
    received_ms: u64,
    declared_duration_ms: u64,
    tolerance_ms: u64,
) -> bool {
    declared_duration_ms > 0 && received_ms + tolerance_ms >= declared_duration_ms
}

#[cfg(test)]
mod body_error_tests {
    use super::{body_error_is_track_end, detect_and_parse};

    const TOL: u64 = 1_000;

    /// Build a WAV header with an explicit data-chunk size, as a progressive
    /// transcode does.
    fn wav_with_data_size(data_size: u32, sample_rate: u32, channels: u16, bits: u16) -> Vec<u8> {
        let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
        let block_align = channels * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&36u32.wrapping_add(data_size).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&sample_rate.to_le_bytes());
        b.extend_from_slice(&byte_rate.to_le_bytes());
        b.extend_from_slice(&block_align.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_size.to_le_bytes());
        b.resize(b.len() + 256, 0);
        b
    }

    /// The sentinel a progressive transcode really writes must parse as
    /// "unknown", not as a duration. Converted naively it looks plausible —
    /// 3 h 23 at 44.1/16/2 — and that fiction is what made the end-of-track
    /// guard compare against nonsense and never fire (Xavier Joly, 8 Aug 2026).
    #[test]
    fn a_sentinel_data_size_parses_as_unknown_duration() {
        for size in [0x7FFF_FFFFu32, 0xFFFF_FFFFu32] {
            let mut buf = wav_with_data_size(size, 44_100, 2, 16);
            let si = detect_and_parse(&mut buf).expect("header should parse");
            assert_eq!(
                si.duration_ms, 0,
                "data_size {size:#X} must read as unknown"
            );
        }
    }

    /// The trap that broke my first attempt at a threshold: at 192/24 the same
    /// sentinel converts to about 31 minutes, a perfectly credible track
    /// length. No duration cutoff can separate the two — only the byte value
    /// can, which is why the check belongs here.
    #[test]
    fn the_sentinel_is_indistinguishable_by_duration_alone() {
        let mut buf = wav_with_data_size(0x7FFF_FFFF, 192_000, 2, 24);
        let si = detect_and_parse(&mut buf).expect("header should parse");
        assert_eq!(si.duration_ms, 0);
    }

    #[test]
    fn a_real_data_size_still_yields_its_duration() {
        // 10 s at 44.1 kHz / 16-bit / stereo.
        let mut buf = wav_with_data_size(44_100 * 4 * 10, 44_100, 2, 16);
        let si = detect_and_parse(&mut buf).expect("header should parse");
        assert_eq!(si.duration_ms, 10_000);
    }

    /// An unknown duration must leave the resume path in charge rather than
    /// guess that every error is the end of the track.
    #[test]
    fn an_unknown_duration_never_counts_as_the_end() {
        assert!(!body_error_is_track_end(195_200, 0, TOL));
    }

    #[test]
    fn a_body_error_just_short_of_the_declared_duration_is_the_end() {
        assert!(body_error_is_track_end(177_870, 177_900, TOL));
    }

    #[test]
    fn a_body_error_exactly_at_the_declared_duration_is_the_end() {
        assert!(body_error_is_track_end(177_900, 177_900, TOL));
    }

    /// Overshooting the declared duration still counts: the prediction can err
    /// on either side.
    #[test]
    fn a_body_error_past_the_declared_duration_is_the_end() {
        assert!(body_error_is_track_end(178_100, 177_900, TOL));
    }

    /// A real mid-track cut must keep the Range-resume path — that is what
    /// rescues a transient network blip without dropping the track.
    #[test]
    fn a_failure_mid_track_is_not_the_end() {
        assert!(!body_error_is_track_end(90_000, 177_900, TOL));
    }

    /// Just outside the tolerance: still a failure, not an end.
    #[test]
    fn a_failure_just_outside_the_tolerance_is_not_the_end() {
        assert!(!body_error_is_track_end(176_899, 177_900, TOL));
    }
}

/// A next track decoded to PCM in the background, ready to be swapped in when
/// the current one reaches its end.
pub(super) struct StagedDirectTrack {
    pub pcm: Vec<u8>,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub channels: u8,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_url: Option<String>,
    pub duration_ms: u64,
}

/// Prépare un fichier local en PCM pour le chemin de lecture directe : lecture
/// intégrale, analyse de l'en-tête, et c'est tout — le contenu part tel quel.
///
/// Renvoie `None` pour tout ce que ce chemin ne sait pas jouer : DSD, FLAC,
/// en-tête illisible, échec de lecture. L'appelant n'enchaîne alors pas, et le
/// poller fait avancer la file comme aujourd'hui.
///
/// Bloquante par nature (le fichier est lu en entier), donc à exécuter hors de
/// la tâche d'envoi des paquets : un fichier de 190 Mo prend un temps sensible,
/// audible comme une coupure s'il était lu en ligne.
pub(super) fn stage_direct_track(
    file_path: &str,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
) -> Option<StagedDirectTrack> {
    let mut buf = std::fs::read(file_path).ok()?;
    let si = detect_and_parse(&mut buf)?;
    if si.format.is_dsd() {
        return None;
    }

    // Le FLAC n'est pas preparable en direct : sa conversion passait par un
    // `ffmpeg` externe que Tune ne livre plus. `None` renvoie l'appelant vers
    // le flux HTTP, ou l'orchestrateur transcode en natif.
    if si.format == AudioFormat::Flac {
        return None;
    }

    let (pcm, format, sample_rate, bits_per_sample, channels) = {
        (
            buf,
            si.format,
            si.sample_rate,
            si.bits_per_sample,
            si.channels.min(8) as u8,
        )
    };

    Some(StagedDirectTrack {
        pcm,
        format,
        sample_rate,
        bits_per_sample,
        channels,
        title,
        artist,
        album,
        cover_url,
        duration_ms: if duration_ms > 0 {
            duration_ms
        } else {
            si.duration_ms
        },
    })
}

/// Can a staged track be swapped in without renegotiating the OAAT stream?
///
/// A format change mid-stream needs a `propose_format` round-trip the direct
/// path does not implement, so a differing next track falls back to the normal
/// stop-and-restart. Same format is the common case within an album, which is
/// precisely where a gap is most audible.
pub(super) fn staged_track_matches(
    staged: &StagedDirectTrack,
    format: AudioFormat,
    sample_rate: u32,
    bits_per_sample: u16,
    channels: u8,
) -> bool {
    staged.format == format
        && staged.sample_rate == sample_rate
        && staged.bits_per_sample == bits_per_sample
        && staged.channels == channels
}

#[cfg(test)]
mod staged_track_tests {
    use super::{AudioFormat, StagedDirectTrack, staged_track_matches};

    fn staged(format: AudioFormat, rate: u32, bits: u16, ch: u8) -> StagedDirectTrack {
        StagedDirectTrack {
            pcm: Vec::new(),
            format,
            sample_rate: rate,
            bits_per_sample: bits,
            channels: ch,
            title: "Atrevido".into(),
            artist: "Orishas".into(),
            album: "A Lo Cubano".into(),
            cover_url: None,
            duration_ms: 237_265,
        }
    }

    /// The common case inside an album — and precisely where a gap is most
    /// audible.
    #[test]
    fn an_identical_format_can_be_swapped_in() {
        let s = staged(AudioFormat::PcmS16le, 44_100, 16, 2);
        assert!(staged_track_matches(
            &s,
            AudioFormat::PcmS16le,
            44_100,
            16,
            2
        ));
    }

    /// Each of these needs a format renegotiation the direct path does not
    /// implement, so the swap must be refused and the normal restart used.
    #[test]
    fn any_format_difference_refuses_the_swap() {
        let s = staged(AudioFormat::PcmS16le, 44_100, 16, 2);
        assert!(
            !staged_track_matches(&s, AudioFormat::PcmS24le, 44_100, 16, 2),
            "different sample format"
        );
        assert!(
            !staged_track_matches(&s, AudioFormat::PcmS16le, 48_000, 16, 2),
            "different sample rate"
        );
        assert!(
            !staged_track_matches(&s, AudioFormat::PcmS16le, 44_100, 24, 2),
            "different bit depth"
        );
        assert!(
            !staged_track_matches(&s, AudioFormat::PcmS16le, 44_100, 16, 1),
            "different channel count"
        );
    }

    /// A 24/192 album track — the case where the whole-file buffer is largest
    /// and a restart costs the most.
    #[test]
    fn a_hires_track_swaps_like_any_other() {
        let s = staged(AudioFormat::PcmS24le, 192_000, 24, 2);
        assert!(staged_track_matches(
            &s,
            AudioFormat::PcmS24le,
            192_000,
            24,
            2
        ));
    }
}

#[cfg(test)]
mod tests_entete_wav {
    use super::*;

    fn entete(magic: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 92];
        v[..magic.len().min(12)].copy_from_slice(&magic[..magic.len().min(12)]);
        v
    }

    #[test]
    fn un_wav_est_reconnu() {
        assert!(entete_est_wav(&entete(b"RIFF\0\0\0\0WAVE")));
    }

    /// Les trois formats qui finissaient chargés entièrement puis jetés.
    #[test]
    fn flac_dsd_et_compresses_basculent_immediatement() {
        assert!(!entete_est_wav(&entete(b"fLaC")), "FLAC part en HTTP");
        assert!(!entete_est_wav(&entete(b"DSD ")), "DSD doit être converti");
        assert!(!entete_est_wav(&entete(b"ID3\x03")), "MP3 non parsé ici");
        assert!(
            !entete_est_wav(&entete(b"ftypM4A ")),
            "AAC/ALAC non parsé ici"
        );
    }

    /// Un RIFF qui n'est pas du WAVE — AVI, RMI — ne doit pas passer.
    #[test]
    fn un_riff_non_wave_ne_passe_pas() {
        assert!(!entete_est_wav(&entete(b"RIFF\0\0\0\0AVI ")));
    }

    /// Un fichier plus court que l'en-tête ne doit ni paniquer ni passer.
    /// `read` peut rendre moins que demandé, et un fichier tronqué existe.
    #[test]
    fn un_entete_trop_court_ne_panique_pas() {
        for n in 0..12 {
            let court = vec![b'R'; n];
            assert!(!entete_est_wav(&court), "n={n}");
        }
        assert!(!entete_est_wav(b""));
    }
}

// ---------------------------------------------------------------------------
// Compteur de trames FLAC — la position VRAIE d'un flux compressé (#2214)
// ---------------------------------------------------------------------------

/// Suit la position réelle, en samples, d'un flux FLAC servi par tranches
/// arbitraires.
///
/// PTS, barre de position et cadence étaient calculés comme si les octets
/// FLAC étaient du PCM : `byte_offset / (rate × bytes_per_frame)`. Or le débit
/// FLAC est variable — la position dérivait selon le taux de compression du
/// morceau, et `sample_offset` n'était jamais incrémenté sur ce chemin
/// (JP Robbe, #2214).
///
/// L'en-tête de chaque trame FLAC porte sa position ABSOLUE — numéro de sample
/// en blocage variable, numéro de trame en blocage fixe — et sa taille de
/// bloc. On scanne donc les octets au fil de l'envoi : chaque en-tête validé
/// (code de synchro, champs réservés, CRC-8) remet la position à sa valeur
/// exacte. Pas d'accumulation, pas de dérive : une trame manquée est rattrapée
/// à la suivante.
///
/// La fenêtre de recouvrement entre deux tranches est de 15 octets — la
/// taille maximale d'un en-tête — pour qu'un en-tête à cheval sur deux
/// paquets ne soit pas perdu.
pub struct CompteurDeTramesFlac {
    /// Queue de la tranche précédente, pour les en-têtes à cheval.
    reste: Vec<u8>,
    /// Position absolue du DÉBUT de la dernière trame vue, en samples.
    position_samples: u64,
    /// Taille de bloc de la dernière trame vue (blocage fixe : sert à
    /// convertir un numéro de trame en samples).
    dernier_blocsize: u64,
    /// Au moins un en-tête valide a été vu.
    synchronise: bool,
}

impl CompteurDeTramesFlac {
    pub fn new() -> Self {
        Self {
            reste: Vec::new(),
            position_samples: 0,
            dernier_blocsize: 4096,
            synchronise: false,
        }
    }

    /// Le flux a-t-il livré au moins un en-tête de trame valide ?
    pub fn est_synchronise(&self) -> bool {
        self.synchronise
    }

    /// Position absolue, en samples, du début de la dernière trame envoyée.
    pub fn position_samples(&self) -> u64 {
        self.position_samples
    }

    /// Avaler une tranche du flux telle qu'elle part sur le réseau.
    pub fn avaler(&mut self, tranche: &[u8]) {
        // Fenêtre = queue précédente + tranche. 15 octets suffisent : un
        // en-tête FLAC fait au plus 16 octets, et il faut qu'il COMMENCE dans
        // la partie déjà vue pour avoir été manqué.
        let mut fenetre = std::mem::take(&mut self.reste);
        fenetre.extend_from_slice(tranche);

        let mut i = 0usize;
        while i + 5 <= fenetre.len() {
            if fenetre[i] == 0xFF && (fenetre[i + 1] & 0xFE) == 0xF8 {
                match lire_entete_de_trame(&fenetre[i..]) {
                    LectureEntete::Valide { position, blocsize } => {
                        self.position_samples = position;
                        self.dernier_blocsize = blocsize;
                        self.synchronise = true;
                        // L'en-tête est validé : on peut sauter au-delà. Le
                        // corps de la trame ne contient pas d'autre en-tête
                        // qui nous concerne — et s'il contient une fausse
                        // synchro, le CRC l'écartera.
                        i += 5;
                        continue;
                    }
                    LectureEntete::Tronque => break, // il en manque : garder la queue
                    LectureEntete::Invalide => {}
                }
            }
            i += 1;
        }
        // Garder la queue non examinée pour la prochaine tranche.
        let garde = fenetre.len().saturating_sub(16).max(i.min(fenetre.len()));
        let debut_queue = garde.min(fenetre.len());
        self.reste = fenetre[debut_queue..].to_vec();
        // Borner la queue : jamais plus de 16 octets.
        if self.reste.len() > 16 {
            let n = self.reste.len();
            self.reste.drain(..n - 16);
        }
    }
}

enum LectureEntete {
    Valide { position: u64, blocsize: u64 },
    Tronque,
    Invalide,
}

/// Lire un en-tête de trame FLAC posé au début de `b`, et rendre la position
/// absolue qu'il déclare. Validation complète : champs réservés, codes
/// interdits, et CRC-8 de l'en-tête — sans quoi les données compressées
/// offrent des fausses synchros en pagaille.
fn lire_entete_de_trame(b: &[u8]) -> LectureEntete {
    if b.len() < 5 {
        return LectureEntete::Tronque;
    }
    let variable = (b[1] & 0x01) == 1;
    let code_bloc = (b[2] >> 4) & 0x0F;
    let code_rate = b[2] & 0x0F;
    // 0 = réservé pour la taille de bloc ; 15 = invalide pour la cadence.
    if code_bloc == 0 || code_rate == 15 {
        return LectureEntete::Invalide;
    }
    // Bit réservé de l'octet 3 : doit être 0.
    if (b[3] & 0x01) != 0 {
        return LectureEntete::Invalide;
    }
    let mut i = 4usize;

    // Numéro codé en UTF-8 étendu (1 à 7 octets).
    let premier = match b.get(i) {
        Some(v) => *v,
        None => return LectureEntete::Tronque,
    };
    let (mut numero, suite): (u64, usize) = match premier {
        0x00..=0x7F => (premier as u64, 0),
        0xC0..=0xDF => ((premier & 0x1F) as u64, 1),
        0xE0..=0xEF => ((premier & 0x0F) as u64, 2),
        0xF0..=0xF7 => ((premier & 0x07) as u64, 3),
        0xF8..=0xFB => ((premier & 0x03) as u64, 4),
        0xFC..=0xFD => ((premier & 0x01) as u64, 5),
        0xFE => (0, 6),
        _ => return LectureEntete::Invalide,
    };
    i += 1;
    for _ in 0..suite {
        let o = match b.get(i) {
            Some(v) => *v,
            None => return LectureEntete::Tronque,
        };
        if (o & 0xC0) != 0x80 {
            return LectureEntete::Invalide;
        }
        numero = (numero << 6) | (o & 0x3F) as u64;
        i += 1;
    }

    // Taille de bloc, selon le code.
    let blocsize: u64 = match code_bloc {
        1 => 192,
        2..=5 => 576u64 << (code_bloc - 2),
        6 => {
            let v = match b.get(i) {
                Some(v) => *v as u64,
                None => return LectureEntete::Tronque,
            };
            i += 1;
            v + 1
        }
        7 => {
            if b.len() < i + 2 {
                return LectureEntete::Tronque;
            }
            let v = ((b[i] as u64) << 8) | b[i + 1] as u64;
            i += 2;
            v + 1
        }
        8..=15 => 256u64 << (code_bloc - 8),
        _ => return LectureEntete::Invalide,
    };
    // Cadence explicite : octets supplémentaires à sauter.
    match code_rate {
        12 => {
            if b.len() < i + 1 {
                return LectureEntete::Tronque;
            }
            i += 1;
        }
        13 | 14 => {
            if b.len() < i + 2 {
                return LectureEntete::Tronque;
            }
            i += 2;
        }
        _ => {}
    }
    // CRC-8 (polynôme 0x07, init 0) sur tout l'en-tête, octet de CRC exclu.
    let crc_lu = match b.get(i) {
        Some(v) => *v,
        None => return LectureEntete::Tronque,
    };
    let mut crc: u8 = 0;
    for &o in &b[..i] {
        crc ^= o;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    if crc != crc_lu {
        return LectureEntete::Invalide;
    }

    // Blocage variable : le numéro EST la position en samples. Blocage fixe :
    // c'est un numéro de trame — la position est numero × blocsize (exact tant
    // que toutes les trames précédentes ont la même taille, ce que le blocage
    // fixe garantit, dernière trame exceptée).
    let position = if variable {
        numero
    } else {
        numero.saturating_mul(blocsize)
    };
    LectureEntete::Valide { position, blocsize }
}

#[cfg(test)]
mod tests_trames_flac {
    use super::CompteurDeTramesFlac;

    // --- Compteur de trames FLAC (#2214) -----------------------------------

    /// Construire un en-tête de trame FLAC valide, CRC compris.
    /// Blocage fixe, blocksize 4096 (code 12 → 256<<4), cadence 44,1 kHz
    /// (code 9), stéréo (code 1), 16 bits (code 4).
    fn entete_de_trame(numero_de_trame: u64) -> Vec<u8> {
        let mut h = vec![0xFF, 0xF8, 0xC9, 0x18];
        // Numéro en UTF-8 étendu — ici < 128 : un seul octet.
        assert!(numero_de_trame < 128, "test limité aux petits numéros");
        h.push(numero_de_trame as u8);
        let mut crc: u8 = 0;
        for &o in &h {
            crc ^= o;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 {
                    (crc << 1) ^ 0x07
                } else {
                    crc << 1
                };
            }
        }
        h.push(crc);
        h
    }

    #[test]
    fn la_position_flac_vient_des_en_tetes_pas_des_octets() {
        let mut c = CompteurDeTramesFlac::new();
        assert!(!c.est_synchronise());

        // Trame 0 puis un corps compressé quelconque, PLUS GROS que la trame :
        // au prorata des octets, la position exploserait.
        let mut flux = entete_de_trame(0);
        flux.extend(std::iter::repeat(0xA5).take(9000));
        c.avaler(&flux);
        assert!(c.est_synchronise());
        assert_eq!(c.position_samples(), 0, "trame 0 = position 0");

        // Trame 3 (on saute la 1 et la 2 : peu importe, la position est ABSOLUE).
        let mut flux2 = entete_de_trame(3);
        flux2.extend(std::iter::repeat(0x5A).take(500));
        c.avaler(&flux2);
        assert_eq!(
            c.position_samples(),
            3 * 4096,
            "blocage fixe : numéro de trame × blocksize — pas un prorata d'octets"
        );
    }

    #[test]
    fn un_en_tete_a_cheval_sur_deux_tranches_est_vu() {
        let mut c = CompteurDeTramesFlac::new();
        let h = entete_de_trame(5);
        // Couper l'en-tête en deux, au milieu.
        c.avaler(&h[..3]);
        assert!(!c.est_synchronise(), "en-tête incomplet : rien à déclarer");
        c.avaler(&h[3..]);
        assert!(
            c.est_synchronise(),
            "la fenêtre de recouvrement doit le recoller"
        );
        assert_eq!(c.position_samples(), 5 * 4096);
    }

    #[test]
    fn une_fausse_synchro_est_ecartee_par_le_crc() {
        let mut c = CompteurDeTramesFlac::new();
        // 0xFF 0xF8 plausible, champs valides, mais CRC faux.
        let mut faux = entete_de_trame(7);
        let n = faux.len();
        faux[n - 1] ^= 0xFF; // CRC corrompu
        c.avaler(&faux);
        assert!(
            !c.est_synchronise(),
            "un CRC faux doit écarter la trame : les données compressées sont \
             pleines de pseudo-synchros"
        );
    }
}
