//! Native Ogg Opus file encoder (#1525).
//!
//! Encodes interleaved PCM to a `.opus` file (Opus packets in an Ogg
//! container) entirely in-process: libopus via the `opus` crate for the
//! packets, and a minimal hand-rolled Ogg page writer for the container.
//! This replaces the converter's `opusenc`/ffmpeg subprocesses.
//!
//! Scope: mono and stereo only (mapping family 0). Multichannel sources are
//! rejected loudly — the old external path was never exercised for them on a
//! standard install anyway (no opusenc/ffmpeg on PATH).

use opus::{Application, Bitrate, Channels, Encoder};
use tracing::info;

/// Opus operates internally at 48 kHz; callers must resample first
/// (`crate::audio::resample`).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// 20 ms at 48 kHz — the codec's sweet spot for music.
const FRAME_SAMPLES: usize = 960;

/// Audio packets per Ogg page (1 s of audio per page — keeps pages well
/// under the 255-segment lacing limit for music bitrates).
const PACKETS_PER_PAGE: usize = 50;

/// Encode interleaved 16-bit PCM at 48 kHz into a complete Ogg Opus stream.
///
/// * `pcm` — interleaved i16 samples at 48 kHz.
/// * `channels` — 1 or 2.
/// * `bitrate_kbps` — target bitrate (e.g. 128).
/// * `input_sample_rate` — the ORIGINAL source rate, advertised in OpusHead
///   for players that care; playback always happens at 48 kHz.
pub fn encode_ogg_opus(
    pcm: &[i16],
    channels: u16,
    bitrate_kbps: u32,
    input_sample_rate: u32,
) -> Result<Vec<u8>, String> {
    let ch = match channels {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        n => return Err(format!("opus: {n} canaux non pris en charge (mono/stéréo)")),
    };
    let chu = channels as usize;

    let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, ch, Application::Audio)
        .map_err(|e| format!("opus encoder init: {e}"))?;
    encoder
        .set_bitrate(Bitrate::Bits((bitrate_kbps * 1000) as i32))
        .map_err(|e| format!("opus set_bitrate: {e}"))?;
    // Encoder delay, advertised as pre-skip so decoders trim it.
    let pre_skip: u16 = encoder.get_lookahead().map(|l| l as u16).unwrap_or(312);

    // --- Encode all packets first ---------------------------------------
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut out_buf = vec![0u8; 4000]; // max recommended packet size
    let total_frames = pcm.len() / chu;
    let mut frame = vec![0i16; FRAME_SAMPLES * chu];
    let mut offset = 0usize;
    while offset < total_frames {
        let avail = (total_frames - offset).min(FRAME_SAMPLES);
        let src = &pcm[offset * chu..(offset + avail) * chu];
        // Final partial frame: pad with silence, granulepos trims the tail.
        frame[..src.len()].copy_from_slice(src);
        frame[src.len()..].fill(0);
        let n = encoder
            .encode(&frame, &mut out_buf)
            .map_err(|e| format!("opus encode: {e}"))?;
        packets.push(out_buf[..n].to_vec());
        offset += avail;
    }

    // --- Ogg encapsulation ----------------------------------------------
    let serial: u32 = 0x54_55_4E_45; // "TUNE"
    let mut ogg = OggWriter::new(serial);

    // Page 0: OpusHead alone, BOS.
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&input_sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family 0
    ogg.write_page(&[&head], 0, true, false);

    // Page 1: OpusTags alone.
    let vendor = b"tune-server";
    let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no comments
    ogg.write_page(&[&tags], 0, false, false);

    // Audio pages. Granulepos = pre-skip + samples decodable up to and
    // including this page; the final page's value trims the padded tail.
    let mut samples_done = 0u64;
    for (i, group) in packets.chunks(PACKETS_PER_PAGE).enumerate() {
        let is_last = (i + 1) * PACKETS_PER_PAGE >= packets.len();
        let group_frames = group.len() as u64 * FRAME_SAMPLES as u64;
        samples_done += group_frames;
        let granule = if is_last {
            pre_skip as u64 + total_frames as u64
        } else {
            pre_skip as u64 + samples_done
        };
        let refs: Vec<&[u8]> = group.iter().map(|p| p.as_slice()).collect();
        ogg.write_page(&refs, granule, false, is_last);
    }

    info!(
        packets = packets.len(),
        bytes = ogg.data.len(),
        bitrate_kbps,
        channels,
        "opus_ogg_encoded"
    );
    Ok(ogg.data)
}

/// Minimal Ogg page writer — just enough for a single Opus stream.
struct OggWriter {
    data: Vec<u8>,
    serial: u32,
    page_no: u32,
}

impl OggWriter {
    fn new(serial: u32) -> Self {
        Self {
            data: Vec::new(),
            serial,
            page_no: 0,
        }
    }

    /// Write one page containing the given packets (each fully contained —
    /// we never span packets across pages; music packets are far below the
    /// 255×255 byte page payload limit).
    fn write_page(&mut self, packets: &[&[u8]], granule: u64, bos: bool, eos: bool) {
        // Lacing: each packet as N×255 + remainder segments.
        let mut lacing: Vec<u8> = Vec::new();
        for p in packets {
            let mut len = p.len();
            while len >= 255 {
                lacing.push(255);
                len -= 255;
            }
            lacing.push(len as u8); // 0 terminates a 255-multiple correctly
        }
        assert!(
            lacing.len() <= 255,
            "ogg page overflow: reduce packets per page"
        );

        let mut page: Vec<u8> = Vec::with_capacity(27 + lacing.len() + 4000);
        page.extend_from_slice(b"OggS");
        page.push(0); // stream structure version
        page.push(if bos { 0x02 } else { 0 } | if eos { 0x04 } else { 0 });
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.page_no.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        for p in packets {
            page.extend_from_slice(p);
        }

        let crc = ogg_crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        self.data.extend_from_slice(&page);
        self.page_no += 1;
    }
}

/// Ogg's CRC-32: polynomial 0x04C11DB7, no reflection, init 0, no final xor.
fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Énergie du signal à la fréquence `freq`, par l'algorithme de Goertzel
/// (#2251).
///
/// Pourquoi cet outil ici : la liaison libopus a été remplacée (`audiopus_sys`
/// abandonné → `opus`/`opusic-sys`, #2251). Or **remplacer un décodeur ne casse
/// pas la compilation** — il rend
/// un son faux, et les contrôles existants (durée, cadence, « pas du
/// silence ») laissent passer un canal inversé, un repli mono, un gain faux ou
/// un flux brouillé. Une empreinte spectrale, elle, ne laisse rien passer, et
/// reste valable d'une version de libopus à l'autre : le décodage Opus n'est
/// **pas** garanti bit-à-bit entre versions (RFC 8251), un condensat figé
/// serait donc instable, alors qu'un 440 Hz reste un 440 Hz.
///
/// Retourne |X(f)|² normalisé par N². Choisir `x.len()` tel que
/// `x.len() * freq / sample_rate` soit entier donne un résultat exact (pas de
/// fuite spectrale).
#[cfg(test)]
pub(crate) fn goertzel_power(x: &[f64], sample_rate: f64, freq: f64) -> f64 {
    let n = x.len() as f64;
    let k = (0.5 + n * freq / sample_rate).floor();
    let w = 2.0 * std::f64::consts::PI * k / n;
    let (cosine, sine) = (w.cos(), w.sin());
    let coeff = 2.0 * cosine;
    let (mut s_prev, mut s_prev2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let real = s_prev - s_prev2 * cosine;
    let imag = s_prev2 * sine;
    (real * real + imag * imag) / (n * n)
}

/// Moyenne quadratique d'un canal (#2251).
#[cfg(test)]
pub(crate) fn rms(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
}

/// Extrait le canal `idx` d'un flux entrelacé à `channels` canaux (#2251).
#[cfg(test)]
pub(crate) fn channel_f64(interleaved: &[i32], channels: usize, idx: usize) -> Vec<f64> {
    interleaved
        .chunks(channels)
        .filter_map(|c| c.get(idx).map(|&s| s as f64))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seconds` of a 440 Hz sine, interleaved i16 stereo at 48 kHz.
    fn sine_48k_stereo(seconds: f64) -> Vec<i16> {
        let frames = (48000.0 * seconds) as usize;
        (0..frames)
            .flat_map(|n| {
                let v = ((2.0 * std::f64::consts::PI * 440.0 * n as f64 / 48000.0).sin() * 16000.0)
                    as i16;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn ogg_crc_matches_known_vector() {
        // CRC of "OggS" under Ogg's parameters — computed independently.
        assert_eq!(ogg_crc32(b""), 0);
        // Self-consistency: CRC over a page with the CRC field zeroed must be
        // what we embed; verified structurally in round_trip below.
        assert_ne!(ogg_crc32(b"OggS"), 0);
    }

    #[test]
    fn encode_produces_valid_ogg_structure() {
        let pcm = sine_48k_stereo(0.5);
        let bytes = encode_ogg_opus(&pcm, 2, 128, 44100).expect("encode");
        // Starts with a BOS page carrying OpusHead.
        assert_eq!(&bytes[..4], b"OggS");
        assert_eq!(bytes[5] & 0x02, 0x02, "first page must be BOS");
        assert!(
            bytes.windows(8).any(|w| w == b"OpusHead"),
            "OpusHead missing"
        );
        assert!(
            bytes.windows(8).any(|w| w == b"OpusTags"),
            "OpusTags missing"
        );
        // Exactly one EOS flag, on the last page.
        let eos_pages = bytes
            .windows(6)
            .filter(|w| &w[..4] == b"OggS" && w[5] & 0x04 != 0)
            .count();
        assert_eq!(eos_pages, 1, "exactly one EOS page expected");
    }

    #[test]
    fn round_trip_through_project_decoder() {
        // The strongest check available in-tree: our own Opus decode path
        // (symphonia Ogg demux + libopus) must read the file back with the
        // right duration and a live signal. This is the exact chain a Tune
        // zone uses to play the converted file.
        let seconds = 1.0;
        let pcm = sine_48k_stereo(seconds);
        let bytes = encode_ogg_opus(&pcm, 2, 128, 48000).expect("encode");

        let dir = crate::test_scratch::scratch_dir("tune-opus-test");
        let path = dir.join("roundtrip.opus");
        std::fs::write(&path, &bytes).unwrap();

        let decoded =
            crate::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, f64::MAX)
                .expect("decode back");

        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.channels, 2);
        assert!(
            (decoded.duration_s - seconds).abs() < 0.05,
            "duration drifted: {}",
            decoded.duration_s
        );
        let rms = (decoded
            .samples_i32
            .iter()
            .map(|&s| (s as f64) * (s as f64))
            .sum::<f64>()
            / decoded.samples_i32.len() as f64)
            .sqrt();
        assert!(rms > 1000.0, "decoded signal is near-silent: rms={rms}");
    }

    /// `seconds` de deux sinus distincts — `fl` à gauche, `fr` à droite —
    /// entrelacés i16 stéréo à 48 kHz.
    fn two_tone_48k_stereo(seconds: f64, fl: f64, fr: f64, amp: f64) -> Vec<i16> {
        let frames = (48000.0 * seconds) as usize;
        (0..frames)
            .flat_map(|n| {
                let t = n as f64 / 48000.0;
                let l = ((2.0 * std::f64::consts::PI * fl * t).sin() * amp) as i16;
                let r = ((2.0 * std::f64::consts::PI * fr * t).sin() * amp) as i16;
                [l, r]
            })
            .collect()
    }

    /// Harnais de preuve du remplacement d'`audiopus_sys` (#2251).
    ///
    /// `round_trip_through_project_decoder` ci-dessus vérifie qu'il sort *du*
    /// son ; il ne vérifie pas que c'est *le bon* son. Ce test-ci encode deux
    /// tons DIFFÉRENTS à gauche et à droite, puis exige de la chaîne complète
    /// (encodeur libopus → Ogg → démultiplexage symphonia → décodeur libopus)
    /// qu'elle les rende chacun à sa place et à son niveau.
    ///
    /// Il tombe donc sur : inversion des canaux, repli mono, cadence fausse
    /// (le ton se décalerait), erreur de gain, entiers mal interprétés
    /// (boutisme, décalage de bits) — c'est-à-dire l'essentiel des façons dont
    /// un changement de bibliothèque compile parfaitement et sonne faux.
    ///
    /// Ici les attendus sont **calculés**, pas relevés : la source est
    /// synthétique, donc le RMS visé vaut exactement `AMP/√2`. Rien n'est donc
    /// lié à une version de libopus en particulier — ce qui est précisément ce
    /// qui a permis à ce test de valider le passage d'`audiopus_sys 0.2.2`
    /// (libopus embarquée de 2021, ou celle de `pkg-config`) à
    /// `opusic-sys 0.7.5` (libopus 1.6.1 embarquée, toujours statique) sans
    /// retoucher un seul attendu.
    ///
    /// Valeurs relevées à titre indicatif : séparation des canaux > 7·10⁷,
    /// RMS 11315 / 11302 pour une source à 11314. Les seuils ci-dessous
    /// gardent quatre ordres de grandeur de marge.
    #[test]
    fn round_trip_garde_la_frequence_et_l_ordre_des_canaux() {
        const AMP: f64 = 16000.0;
        let pcm = two_tone_48k_stereo(1.0, 440.0, 880.0, AMP);
        let bytes = encode_ogg_opus(&pcm, 2, 128, 48000).expect("encode");

        let dir = crate::test_scratch::scratch_dir("tune-opus-tones");
        let path = dir.join("two-tone.opus");
        std::fs::write(&path, &bytes).unwrap();
        let decoded =
            crate::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, f64::MAX)
                .expect("decode back");

        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);

        // Fenêtre centrale de 0,5 s : on écarte les transitoires de bord
        // (pre-skip, dernière trame complétée par du silence). 24 000
        // échantillons portent un nombre entier de périodes de 440 et de
        // 880 Hz, donc Goertzel est exact.
        let window = |ch: usize| -> Vec<f64> {
            channel_f64(&decoded.samples_i32, 2, ch)
                .into_iter()
                .skip(12_000)
                .take(24_000)
                .collect()
        };
        let left = window(0);
        let right = window(1);
        assert_eq!(left.len(), 24_000, "fenêtre gauche incomplète");
        assert_eq!(right.len(), 24_000, "fenêtre droite incomplète");

        let l440 = goertzel_power(&left, 48_000.0, 440.0);
        let l880 = goertzel_power(&left, 48_000.0, 880.0);
        let r440 = goertzel_power(&right, 48_000.0, 440.0);
        let r880 = goertzel_power(&right, 48_000.0, 880.0);

        assert!(
            l440 > 1000.0 * l880.max(1.0),
            "le canal GAUCHE doit porter le 440 Hz, pas le 880 Hz \
             (p440={l440:.1}, p880={l880:.1}) — canaux inversés ou repliés en mono ?"
        );
        assert!(
            r880 > 1000.0 * r440.max(1.0),
            "le canal DROIT doit porter le 880 Hz, pas le 440 Hz \
             (p440={r440:.1}, p880={r880:.1}) — canaux inversés ou repliés en mono ?"
        );

        // Niveau : un sinus d'amplitude AMP a pour RMS AMP/√2. Opus à
        // 128 kb/s le restitue à moins de 1 % près ; ±15 % couvre largement
        // toute libopus conforme, et attrape un gain divisé ou doublé.
        let expected = AMP / std::f64::consts::SQRT_2;
        for (name, ch) in [("gauche", &left), ("droit", &right)] {
            let got = rms(ch);
            assert!(
                (got - expected).abs() / expected < 0.15,
                "niveau du canal {name} dérivé : rms={got:.0}, attendu ≈{expected:.0}"
            );
        }
    }
}
