use std::collections::HashMap;
use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use rubato::Resampler;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;
use tokio::sync::mpsc;
use tracing::{debug, error};

use super::dsd_to_pcm::choose_output_rate;

/// How long a decoder waits for the consumer to take a chunk before giving up.
///
/// Generous on purpose: a consumer can legitimately stop reading for a long
/// while — OAAT prefetches the next track, reads its header, then leaves the
/// body untouched until the current track ends.
///
/// Named rather than spelled out at each site because the value has already
/// drifted once: the log messages announced `_10s` long after it became 300,
/// which cost a wrong diagnosis on #1323. A name cannot go stale; a number in
/// a message can.
const SEND_TIMEOUT_SECS: u64 = 300;

/// Round a preferred PCM batch size down to a whole number of interleaved frames.
///
/// The streaming decode paths used to drain a fixed `32768` bytes. That length is
/// frame-aligned for 16-bit stereo (`32768 % 4 == 0`) and 32-bit stereo
/// (`32768 % 8 == 0`), but **not** for 24-bit stereo (`32768 % 6 == 2`). Audio
/// consumers concatenate chunks so playback stays fine; per-chunk VU analysis
/// (`compute_levels` / `send_windowed_pcm`) does not — after the first batch the
/// sample boundaries are shifted and peaks peg near 0 dBFS.
fn frame_aligned_chunk_len(preferred: usize, bit_depth: u16, channels: u16) -> usize {
    let frame = (bit_depth as usize / 8).saturating_mul(channels.max(1) as usize);
    if frame == 0 {
        return preferred;
    }
    let aligned = (preferred / frame) * frame;
    if aligned == 0 { frame } else { aligned }
}

/// Resolve the actual audio bit depth from codec parameters.
///
/// Symphonia's ISOMP4 demuxer does not populate `bits_per_sample` for ALAC
/// tracks (only PCM codecs get it). The ALAC decoder also doesn't propagate
/// the value from the magic cookie to the codec parameters. This leaves
/// `bits_per_sample == None` for ALL ALAC files, regardless of actual depth.
///
/// When `bits_per_sample` is absent, this function inspects the codec's
/// `extra_data` (the ALAC magic cookie) to extract the true bit depth.
/// The cookie layout has `bit_depth` at byte offset 5 (0-indexed) within
/// the 24-byte payload.
///
/// Without this fix, 24-bit ALAC files are decoded as 16-bit, producing a
/// WAV stream whose PCM data mismatches the header — causing silence or
/// errors on DLNA renderers.
fn resolve_bit_depth(params: &AudioCodecParameters) -> u16 {
    if let Some(bps) = params.bits_per_sample {
        return bps as u16;
    }

    // ALAC magic cookie: the raw extra_data may be 24 or 48 bytes.
    // Skip optional `frma` (12 bytes) and `alac` (12 bytes) atom prefixes,
    // then byte 5 of the remaining 24-byte payload is the bit depth.
    if let Some(ref extra) = params.extra_data {
        let mut buf: &[u8] = extra;

        // Skip optional frma atom prefix
        if buf.len() >= 12 && &buf[4..8] == b"frma" {
            buf = &buf[12..];
        }
        // Skip optional alac atom prefix
        if buf.len() >= 12 && &buf[4..8] == b"alac" {
            buf = &buf[12..];
        }

        if buf.len() >= 24 {
            let bd = buf[5];
            if bd > 0 && bd <= 32 {
                debug!(
                    bit_depth = bd,
                    "resolved_bit_depth_from_extra_data (ALAC magic cookie)"
                );
                return bd as u16;
            }
        }
    }

    // Ultimate fallback
    16
}

/// La profondeur du **conteneur** PCM qui portera une source de `bd` bits.
///
/// Tout ce qui est en aval — l'en-tête WAV, `convert_pcm_bit_depth`,
/// `StreamingPcmByteAdapter` — ne sait écrire que 16, 24 ou 32 bits, et le
/// contrôle existait déjà sur la **cible** (`stream target bit depth …`,
/// `PCM source bit depth …`) mais nulle part sur la profondeur **lue dans le
/// fichier** : `resolve_bit_depth` rend `bits_per_sample` sans le borner.
///
/// On arrondit vers le **haut**, jamais vers le bas : le décalage de
/// droitisation devient `32 - conteneur`, si bien qu'une source de 20 bits est
/// droitisée sur 24 avec quatre zéros en poids faibles. Aucun bit n'est perdu,
/// et la largeur annoncée redevient celle des octets réellement écrits — c'est
/// ce désaccord-là qui faisait lire des trames de 32 bits dans des octets de
/// 16 (#2157).
fn container_bit_depth(bd: u16) -> u16 {
    match bd {
        0..=16 => 16,
        17..=24 => 24,
        _ => 32,
    }
}

/// Rebuild the decoder after `Error::ResetRequired` from `next_packet`.
///
/// A chained Ogg (an icecast rip, or two files joined with `cat`) contains a
/// mid-file `is_first_page`: symphonia's OggReader starts a new physical
/// stream, rebuilds its track list and returns `ResetRequired`. The consumer
/// must re-fetch the default track, build a fresh decoder and keep pulling
/// packets. Treating it as EOF truncated playback to the first chain link —
/// the local output then signalled a natural end a few seconds in and the
/// poller replayed the track: the « boucle sur les 2-3 premières secondes »
/// of #1270 (liste Bertrand 13/08).
///
/// Returns the new `(track_id, decoder)` when the next link is decodable and
/// keeps the same sample rate / channel count. A mid-stream parameter change
/// cannot be represented in the already-announced PCM stream, so those (and
/// codec init failures) return `None`: stop decoding at the boundary, which is
/// the pre-fix behaviour.
fn rebuild_decoder_after_ogg_chain_reset(
    format: &dyn FormatReader,
    file_path: &str,
    expect_rate: u32,
    expect_channels: u32,
) -> Option<(u32, Box<dyn AudioDecoder>)> {
    let track = format.default_track(TrackType::Audio)?;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => return None,
    };
    let rate = params.sample_rate.unwrap_or(44100);
    let channels = params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(2);
    if rate != expect_rate || channels != expect_channels {
        tracing::warn!(
            file = file_path,
            expect_rate,
            rate,
            expect_channels,
            channels,
            "ogg_chain_params_changed_stopping_at_boundary"
        );
        return None;
    }
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .ok()?;
    debug!(
        file = file_path,
        track_id = track.id,
        "ogg_chain_decoder_rebuilt"
    );
    Some((track.id, decoder))
}

pub struct DecodedAudio {
    pub samples_i32: Vec<i32>,
    pub bit_depth: u16,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration_s: f64,
}

impl DecodedAudio {
    pub fn pcm_bytes(&self) -> Vec<u8> {
        match self.bit_depth {
            24 => self
                .samples_i32
                .iter()
                .flat_map(|s| {
                    let b = s.to_le_bytes();
                    [b[0], b[1], b[2]].into_iter()
                })
                .collect(),
            32 => self
                .samples_i32
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
            _ => self
                .samples_i32
                .iter()
                .flat_map(|s| (*s as i16).to_le_bytes())
                .collect(),
        }
    }
}

fn checked_channels(channels: u32, context: &str) -> Result<u16, String> {
    if !(1..=32).contains(&channels) {
        return Err(format!(
            "{context}: unsupported channel count {channels} (expected 1..=32)"
        ));
    }
    Ok(channels as u16)
}

/// Apply the exact rate/channel contract after a native whole-file decode.
fn adapt_decoded_audio(
    decoded: DecodedAudio,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
) -> Result<DecodedAudio, String> {
    if target_sample_rate == Some(0) {
        return Err("requested PCM sample rate must be greater than zero".into());
    }
    if let Some(channels) = target_channels {
        checked_channels(channels, "requested PCM format")?;
    }
    if decoded.sample_rate == 0 {
        return Err("decoded PCM has a zero sample rate".into());
    }
    let source_channels = checked_channels(decoded.channels, "decoded PCM")?;
    let output_channels = checked_channels(
        target_channels.unwrap_or(decoded.channels),
        "requested PCM format",
    )?;
    let output_rate = target_sample_rate.unwrap_or(decoded.sample_rate);
    if output_rate == 0 {
        return Err("requested PCM sample rate must be greater than zero".into());
    }

    let mut samples = super::channels::adapt_channels_i32(
        &decoded.samples_i32,
        source_channels,
        output_channels,
        decoded.bit_depth,
    )?;
    if decoded.sample_rate != output_rate {
        samples = super::resample::resample_i32(
            &samples,
            decoded.bit_depth,
            output_channels,
            decoded.sample_rate,
            output_rate,
        );
    }
    if samples.len() % output_channels as usize != 0 {
        return Err(format!(
            "adapted PCM sample count {} is not aligned to {output_channels} channels",
            samples.len()
        ));
    }
    let frames = samples.len() / output_channels as usize;
    let duration_s = frames as f64 / output_rate as f64;
    debug!(
        source_rate = decoded.sample_rate,
        output_rate, source_channels, output_channels, frames, "decoded_pcm_contract_applied"
    );
    Ok(DecodedAudio {
        samples_i32: samples,
        bit_depth: decoded.bit_depth,
        sample_rate: output_rate,
        channels: output_channels as u32,
        duration_s,
    })
}

/// Stateful rate/channel adapter for progressive decode paths.
struct StreamingPcmAdapter {
    bit_depth: u16,
    source_channels: u16,
    output_channels: u16,
    source_rate: u32,
    output_rate: u32,
    resampler: Option<rubato::Async<f32>>,
    resample_leftover: Vec<f32>,
    resampled_pending: Vec<f32>,
    resampler_delay_remaining: usize,
    source_frames_seen: u64,
    output_frames_emitted: u64,
}

/// Stateful adapter for progressively received interleaved integer PCM bytes.
///
/// Network chunks are not required to end on a sample or frame boundary.  The
/// incomplete tail is therefore retained until the next call, while complete
/// frames go through the same channel/rate adapter as file decoding.  A final
/// partial frame is an invalid payload and is reported instead of silently
/// truncating it.
pub(crate) struct StreamingPcmByteAdapter {
    pcm: StreamingPcmAdapter,
    source_bit_depth: u16,
    target_bit_depth: u16,
    source_frame_bytes: usize,
    source_leftover: Vec<u8>,
}

impl StreamingPcmAdapter {
    fn new(
        bit_depth: u16,
        source_channels: u32,
        output_channels: u32,
        source_rate: u32,
        output_rate: u32,
    ) -> Result<Self, String> {
        let source_channels = checked_channels(source_channels, "stream source")?;
        let output_channels = checked_channels(output_channels, "stream target")?;
        let resampler = if source_rate != output_rate {
            Some(super::resample::new_streaming_resampler(
                source_rate,
                output_rate,
                output_channels,
            )?)
        } else {
            None
        };
        let resampler_delay_remaining = resampler
            .as_ref()
            .map(Resampler::output_delay)
            .unwrap_or_default();
        Ok(Self {
            bit_depth,
            source_channels,
            output_channels,
            source_rate,
            output_rate,
            resampler,
            resample_leftover: Vec::new(),
            resampled_pending: Vec::new(),
            resampler_delay_remaining,
            source_frames_seen: 0,
            output_frames_emitted: 0,
        })
    }

    fn push(&mut self, samples: &[i32]) -> Result<Vec<i32>, String> {
        let remixed = super::channels::adapt_channels_i32(
            samples,
            self.source_channels,
            self.output_channels,
            self.bit_depth,
        )?;
        self.source_frames_seen += (remixed.len() / self.output_channels as usize) as u64;
        self.resample(&remixed, false)
    }

    fn finish(&mut self) -> Result<Vec<i32>, String> {
        self.resample(&[], true)
    }

    fn resample(&mut self, samples: &[i32], flush: bool) -> Result<Vec<i32>, String> {
        if self.resampler.is_none() {
            return Ok(samples.to_vec());
        }
        let depth = self.bit_depth.clamp(8, 32);
        let full_scale = (1i64 << (depth - 1)) as f32;
        let normalized: Vec<f32> = samples.iter().map(|&s| s as f32 / full_scale).collect();
        let mut output = super::resample::rubato_resample_chunk(
            &mut self.resampler,
            &normalized,
            self.output_channels,
            flush,
            &mut self.resample_leftover,
        );

        // A sinc resampler emits its group delay at the head and a padded tail
        // when flushed.  Those frames are latency, not programme material: if
        // forwarded they lengthen every converted track and make OAAT PTS and
        // duration disagree.  Drop the delay, then expose at most the exact
        // number of frames justified by the source seen so far.
        let channels = self.output_channels as usize;
        let available_frames = output.len() / channels;
        let skipped = self.resampler_delay_remaining.min(available_frames);
        if skipped > 0 {
            output.drain(..skipped * channels);
            self.resampler_delay_remaining -= skipped;
        }
        self.resampled_pending.extend(output);

        let ratio = self.output_rate as f64 / self.source_rate as f64;
        let expected_total = if flush {
            (self.source_frames_seen as f64 * ratio).round() as u64
        } else {
            (self.source_frames_seen as f64 * ratio).floor() as u64
        };
        let allowed = expected_total.saturating_sub(self.output_frames_emitted) as usize;
        let emitted_frames = allowed.min(self.resampled_pending.len() / channels);
        let emitted_samples = emitted_frames * channels;
        let emitted: Vec<f32> = self.resampled_pending.drain(..emitted_samples).collect();
        self.output_frames_emitted += emitted_frames as u64;

        if flush {
            if self.output_frames_emitted != expected_total {
                return Err(format!(
                    "streaming resampler emitted {} frame(s), expected {expected_total}",
                    self.output_frames_emitted
                ));
            }
            // Any remaining frames belong to the padded filter tail.
            self.resampled_pending.clear();
        }

        let max = full_scale - 1.0;
        Ok(emitted
            .into_iter()
            .map(|sample| (sample * full_scale).clamp(-full_scale, max) as i32)
            .collect())
    }
}

impl StreamingPcmByteAdapter {
    pub(crate) fn new(
        source_bit_depth: u16,
        source_channels: u32,
        source_rate: u32,
        target_bit_depth: u16,
        target_channels: u32,
        target_rate: u32,
    ) -> Result<Self, String> {
        if !matches!(source_bit_depth, 16 | 24 | 32) {
            return Err(format!(
                "PCM source bit depth {source_bit_depth} is unsupported (expected 16, 24 or 32)"
            ));
        }
        if !matches!(target_bit_depth, 16 | 24 | 32) {
            return Err(format!(
                "PCM target bit depth {target_bit_depth} is unsupported (expected 16, 24 or 32)"
            ));
        }
        if source_rate == 0 || target_rate == 0 {
            return Err("PCM sample rates must be greater than zero".into());
        }
        let source_channels_checked = checked_channels(source_channels, "PCM byte source")?;
        checked_channels(target_channels, "PCM byte target")?;
        let source_frame_bytes = (source_bit_depth as usize / 8)
            .checked_mul(source_channels_checked as usize)
            .ok_or("PCM source frame size overflow")?;
        let pcm = StreamingPcmAdapter::new(
            source_bit_depth,
            source_channels,
            target_channels,
            source_rate,
            target_rate,
        )?;
        Ok(Self {
            pcm,
            source_bit_depth,
            target_bit_depth,
            source_frame_bytes,
            source_leftover: Vec::new(),
        })
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.source_leftover.extend_from_slice(bytes);
        let complete_len =
            self.source_leftover.len() - (self.source_leftover.len() % self.source_frame_bytes);
        if complete_len == 0 {
            return Ok(Vec::new());
        }

        let tail = self.source_leftover.split_off(complete_len);
        let complete = std::mem::replace(&mut self.source_leftover, tail);
        let samples = pcm_bytes_to_i32(&complete, self.source_bit_depth)?;
        let adapted = self.pcm.push(&samples)?;
        Ok(convert_pcm_bit_depth(
            &adapted,
            self.source_bit_depth,
            self.target_bit_depth,
        ))
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, String> {
        if !self.source_leftover.is_empty() {
            return Err(format!(
                "PCM stream ended with {} byte(s) outside a complete {}-byte source frame",
                self.source_leftover.len(),
                self.source_frame_bytes
            ));
        }
        let adapted = self.pcm.finish()?;
        Ok(convert_pcm_bit_depth(
            &adapted,
            self.source_bit_depth,
            self.target_bit_depth,
        ))
    }
}

fn pcm_bytes_to_i32(data: &[u8], bit_depth: u16) -> Result<Vec<i32>, String> {
    let samples = match bit_depth {
        16 => data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect(),
        24 => data
            .chunks_exact(3)
            .map(|b| {
                let value = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                (value << 8) >> 8
            })
            .collect(),
        32 => data
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        _ => return Err(format!("unsupported PCM bit depth: {bit_depth}")),
    };
    Ok(samples)
}

/// Requantifier un échantillon **droitisé** de `from_bd` vers `to_bd` bits.
///
/// Droitisé veut dire qu'un échantillon de 24 bits occupe les bits 0..23 du
/// `i32`. Passer d'une profondeur à l'autre est donc un décalage — et **ce
/// décalage EST le niveau**. L'omettre ne perd pas un bit de poids faible : il
/// laisse l'échantillon `to_bd - from_bd` rangs trop bas, c'est-à-dire une
/// division par `2^(to_bd - from_bd)`.
///
/// La table précédente n'énumérait que 16, 24 et 32 et rendait l'échantillon
/// **inchangé** pour toute autre profondeur source. Or `resolve_bit_depth` rend
/// `bits_per_sample` tel quel, et FLAC, WAV, AIFF et WavPack déclarent
/// légalement 8, 12 ou 20 bits. La sortie locale (cpal/WASAPI) est la seule à
/// demander `to_bd = 32` : une source de 20 bits y sortait donc `2^12` fois
/// trop bas, soit **−72 dB** — audible mais noyé, exactement « son très très
/// faible quasi inaudible » (#2157). En 16 bits de sortie, le même trou faisait
/// pire : `*s as i16` tronquait un mot de 20 bits et **repliait** le signal.
fn requantize(sample: i32, from_bd: u16, to_bd: u16) -> i32 {
    let from = from_bd.clamp(1, 32);
    let to = to_bd.clamp(1, 32);
    if to >= from {
        sample << (to - from)
    } else {
        sample >> (from - to)
    }
}

/// Convert right-justified i32 samples from one bit depth to another,
/// producing raw PCM bytes at the target depth.
///
/// Source samples are assumed to be right-justified (i.e. a 24-bit sample
/// occupies bits 0..23 of the i32, a 16-bit sample occupies bits 0..15).
pub(super) fn convert_pcm_bit_depth(samples: &[i32], from_bd: u16, to_bd: u16) -> Vec<u8> {
    match to_bd {
        24 => samples
            .iter()
            .map(|s| {
                let b = requantize(*s, from_bd, 24).to_le_bytes();
                [b[0], b[1], b[2]]
            })
            .flat_map(|a| a.into_iter())
            .collect(),
        32 => samples
            .iter()
            .map(|s| requantize(*s, from_bd, 32).to_le_bytes())
            .flat_map(|a| a.into_iter())
            .collect(),
        _ => {
            // 16-bit output
            samples
                .iter()
                .flat_map(|s| (requantize(*s, from_bd, 16) as i16).to_le_bytes())
                .collect()
        }
    }
}

fn append_pcm_samples(output: &mut Vec<u8>, samples: &[i32], from_bd: u16, to_bd: u16) {
    output.extend_from_slice(&convert_pcm_bit_depth(samples, from_bd, to_bd));
}

/// Convert interleaved raw PCM **bytes** from `from_bd` to `to_bd` bit depth.
///
/// The prefetch buffer stores decoded PCM at the source bit depth (e.g. 16-bit
/// for a Qobuz 16/44 track). When that buffer is served to a local output —
/// which expects 32-bit — the bytes must be widened, otherwise the device reads
/// 32-bit frames out of 16-bit data and plays white noise (Bilou: "bruit blanc"
/// on next-track for a Qobuz album on Windows local output).
pub fn convert_pcm_bytes(data: &[u8], from_bd: u16, to_bd: u16) -> Vec<u8> {
    if from_bd == to_bd {
        return data.to_vec();
    }
    let samples: Vec<i32> = match from_bd {
        16 => data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect(),
        24 => data
            .chunks_exact(3)
            .map(|b| {
                // sign-extend 24-bit LE into i32
                let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                (v << 8) >> 8
            })
            .collect(),
        32 => data
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        _ => return data.to_vec(),
    };
    convert_pcm_bit_depth(&samples, from_bd, to_bd)
}

pub fn can_decode_native(file_path: &str) -> bool {
    super::support::native_decoder_supports_file(Path::new(file_path))
}

fn reject_unsupported_library_audio(file_path: &str) -> Result<(), String> {
    match super::support::decoder_rejection(Path::new(file_path)) {
        Some(unsupported) => Err(format!(
            "format non pris en charge : {} ({})",
            unsupported.report_key, unsupported.reason
        )),
        None => Ok(()),
    }
}

fn is_wavpack(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    ext == "wv"
}

/// A source file staged to a fast local temp so the decoder doesn't do
/// seek-heavy reads over a slow network mount. Shared via `Arc` through the
/// staging cache; the temp is removed when the LAST holder drops (an active
/// decoder keeps it alive even after the cache evicts it).
struct StagedFile {
    path: PathBuf,
    bytes: u64,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Identité d'un fichier source pour le cache de staging : chemin + date +
/// taille. Un fichier modifié (mtime/taille différents) est une entrée neuve —
/// on ne resert jamais une copie périmée.
#[derive(Clone, PartialEq, Eq, Hash)]
struct StageKey {
    src: String,
    mtime: i64,
    size: u64,
}

/// Cache de fichiers stagés, borné en octets, éviction LRU.
///
/// Copier un fichier réseau (NAS WiFi d'Yves : ALAC de 17 à 152 Mo) coûte de
/// 3 à 42 s. Sans cache, chaque lecture — et même deux demandes concurrentes
/// de la MÊME piste (double-staging observé, deux copies à 1 ms d'écart) —
/// re-payait la copie : 1,3 Go de trafic réseau en une session. Le cache copie
/// chaque fichier AU PLUS UNE FOIS ; le single-flight fait attendre les
/// demandes concurrentes au lieu de recopier.
struct StageCache {
    map: HashMap<StageKey, Arc<StagedFile>>,
    /// Ordre d'usage, du plus ancien (avant) au plus récent (arrière).
    lru: Vec<StageKey>,
    /// Total des octets DÉTENUS par le cache (approximatif après éviction d'une
    /// entrée encore en cours de décodage : le fichier survit via l'Arc du
    /// décodeur mais on cesse de le compter).
    bytes: u64,
    budget: u64,
    /// Verrous par clé : un seul thread copie une clé donnée, les autres
    /// attendent son résultat.
    inflight: HashMap<StageKey, Arc<Mutex<()>>>,
}

impl StageCache {
    fn new(budget: u64) -> Self {
        Self {
            map: HashMap::new(),
            lru: Vec::new(),
            bytes: 0,
            budget,
            inflight: HashMap::new(),
        }
    }

    fn touch(&mut self, key: &StageKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            let k = self.lru.remove(pos);
            self.lru.push(k);
        }
    }

    /// Insère une entrée fraîchement copiée et évince les plus anciennes tant
    /// que le budget est dépassé. Évincer ne supprime pas le fichier tout de
    /// suite : on lâche seulement la référence du cache ; un décodeur qui tient
    /// encore l'Arc garde le temp vivant jusqu'à la fin de sa lecture.
    fn insert(&mut self, key: StageKey, entry: Arc<StagedFile>) {
        self.bytes = self.bytes.saturating_add(entry.bytes);
        self.map.insert(key.clone(), entry);
        self.lru.push(key);
        while self.bytes > self.budget && self.lru.len() > 1 {
            let oldest = self.lru.remove(0);
            if let Some(e) = self.map.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(e.bytes);
            }
        }
    }
}

/// Budget disque du cache de staging (octets). Assez pour une longue session
/// d'écoute ALAC (celle d'Yves : 1,3 Go), borné pour ne pas saturer le disque
/// système. Surchargeable par `TUNE_STAGE_CACHE_BYTES`.
fn stage_cache_budget() -> u64 {
    std::env::var("TUNE_STAGE_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3 * 1024 * 1024 * 1024) // 3 Go
}

static STAGE_CACHE: LazyLock<Mutex<StageCache>> =
    LazyLock::new(|| Mutex::new(StageCache::new(stage_cache_budget())));

/// If `src` lives on a different device than the OS temp dir (i.e. an external
/// or network mount), copy it to a local temp file and return a guard.
///
/// Decoders — especially m4a/ALAC via Symphonia — do many small seeks (moov
/// atoms, sample tables). Over SMB/WiFi each seek is a network round-trip, so a
/// few-second decode balloons to 90s+ (Yves: ALAC on a NAS, Mac on WiFi). One
/// sequential copy is fast even on WiFi; decoding then happens from the local
/// copy. Returns `None` (decode the original in place) for same-device files or
/// on any error — never fatal.
/// Le montage qui porte `cible` est-il un système de fichiers RÉSEAU,
/// d'après un contenu de `/proc/mounts` ?
///
/// Fonction PURE, hors de tout `#[cfg]` : compilée et testée sur toutes les
/// plateformes. Un bloc `cfg(linux)` ne serait ni compilé ni testé depuis
/// macOS — c'est ainsi qu'une fonction morte a déjà été livrée (#2277).
/// Le point de montage le plus LONG qui préfixe le chemin gagne.
fn montage_reseau_depuis_mounts(mounts: &str, cible: &str) -> bool {
    const TYPES_RESEAU: &[&str] = &[
        "nfs",
        "nfs4",
        "cifs",
        "smbfs",
        "smb2",
        "webdav",
        "davfs",
        "sshfs",
        "fuse.sshfs",
        "afpfs",
        "9p",
        "ncpfs",
        "glusterfs",
        "cephfs",
        "curlftpfs",
    ];

    let mut meilleur: Option<(usize, String)> = None;
    for ligne in mounts.lines() {
        let mut champs = ligne.split_whitespace();
        let (Some(_dev), Some(point), Some(genre)) = (champs.next(), champs.next(), champs.next())
        else {
            continue;
        };
        // /proc/mounts échappe les espaces en \040.
        let point = point.replace("\\040", " ");
        let prefixe_ok = cible == point
            || (cible.starts_with(&point)
                && (point == "/" || cible.as_bytes().get(point.len()) == Some(&b'/')));
        if prefixe_ok {
            let l = point.len();
            if meilleur.as_ref().map(|(bl, _)| l > *bl).unwrap_or(true) {
                meilleur = Some((l, genre.to_string()));
            }
        }
    }
    meilleur
        .map(|(_, genre)| {
            TYPES_RESEAU
                .iter()
                .any(|t| genre == *t || genre.starts_with("fuse."))
                && genre != "fuseblk"
        })
        .unwrap_or(false)
}

/// Le chemin vit-il sur un montage RÉSEAU ?
///
/// Décision par le type de système de fichiers du point de montage, lu dans
/// `/proc/mounts` (Linux/Android) ou via `statfs` (plateformes Apple). En cas de doute — type
/// inconnu, lecture impossible — on répond `false` : ne pas copier est le
/// choix le moins coûteux, le décodeur lira sur place.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn chemin_sur_montage_reseau(chemin: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    montage_reseau_depuis_mounts(&mounts, &chemin.to_string_lossy())
}

#[cfg(target_vendor = "apple")]
fn chemin_sur_montage_reseau(chemin: &Path) -> bool {
    // Darwin : statfs donne le nom du système de fichiers.
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let Ok(c) = CString::new(chemin.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut st = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
        return false;
    }
    let st = unsafe { st.assume_init() };
    let genre = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .to_lowercase();
    const TYPES_RESEAU_MAC: &[&str] = &["nfs", "smbfs", "webdav", "afpfs", "cifs", "ftp", "9p"];
    TYPES_RESEAU_MAC.iter().any(|t| genre == *t)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn chemin_sur_montage_reseau(_chemin: &Path) -> bool {
    false
}

#[cfg(unix)]
fn stage_locally_for_decode(src: &str) -> Option<Arc<StagedFile>> {
    let src_path = Path::new(src);
    let tmp_dir = std::env::temp_dir();
    // ⚠️ Le critère est le TYPE de montage, pas le numéro de périphérique.
    // `st_dev` diffère pour TOUT point de montage distinct — y compris un
    // disque local dédié, souvent PLUS rapide que le disque système. Sur la
    // .18, chaque piste de /data/music (sdb1, 84 % plein) était recopiée en
    // entier vers /tmp avant la moindre note — des .dsf de 300 Mo — alors que
    // le décodage séquentiel lit le disque local aussi vite que la copie.
    // Seuls les montages RÉSEAU (nfs, cifs/smb, sshfs, webdav…) paient des
    // allers-retours par seek et justifient la copie préalable (Yves, NAS
    // en WiFi : 90 s et plus par piste sans elle).
    if !chemin_sur_montage_reseau(src_path) {
        return None; // stockage local : le décodeur lit sur place
    }

    // Staging PIPELINÉ (phase 2, flag TUNE_STAGE_STREAM_DECODE) : au lieu de
    // copier tout le fichier avant de décoder, on lance la copie en tâche de
    // fond et on rend la main tout de suite ; `decode_symphonia` décode via une
    // source seekable qui suit la frontière d'écriture. Pour un faststart, la
    // première note arrive en 2-3 s au lieu des 41 s d'un gros ALAC. Le temp
    // n'entre PAS dans le cache d'octets (il n'est pas complet à cet instant).
    if crate::audio::staged_growth::stream_decode_enabled()
        && let Ok(m) = std::fs::metadata(src_path)
    {
        let ext = src_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let dst = tmp_dir.join(format!("tune-stage-{}.{ext}", uuid::Uuid::new_v4()));
        let growth = crate::audio::staged_growth::StageGrowth::new(m.len());
        crate::audio::staged_growth::register(&dst.to_string_lossy(), growth.clone());
        let src_owned = src.to_string();
        let dst_owned = dst.clone();
        std::thread::spawn(move || {
            copier_en_fond(&src_owned, &dst_owned, &growth);
        });
        tracing::info!(src = %src, staged = %dst.display(), "decode_source_staging_pipelined");
        return Some(Arc::new(StagedFile {
            path: dst,
            bytes: 0,
        }));
    }

    // Identité du fichier : chemin + date + taille. Une source dont on n'a pas
    // les métadonnées ne peut pas être mise en cache sûrement — on la copie
    // sans cache (comportement historique) plutôt que de risquer une copie
    // périmée.
    let key = match std::fs::metadata(src_path) {
        Ok(m) => Some(StageKey {
            src: src.to_string(),
            mtime: mtime_secs(&m),
            size: m.len(),
        }),
        Err(_) => None,
    };

    // Cache : déjà stagé ? On resert la copie, aucun octet réseau.
    if let Some(ref k) = key {
        let mut cache = STAGE_CACHE.lock().unwrap();
        if let Some(entry) = cache.map.get(k).cloned() {
            cache.touch(k);
            tracing::debug!(src = %src, "decode_source_stage_cache_hit");
            return Some(entry);
        }
    }

    // Single-flight : un seul thread copie une clé donnée. Les demandes
    // concurrentes de la MÊME piste (double-staging observé chez Yves) prennent
    // ce verrou et récupèrent le résultat au lieu de recopier.
    let flight = key.as_ref().map(|k| {
        STAGE_CACHE
            .lock()
            .unwrap()
            .inflight
            .entry(k.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    });
    let _flight_guard = flight.as_ref().map(|m| m.lock().unwrap());

    // Le gagnant du verrou a pu remplir le cache pendant qu'on attendait.
    if let Some(ref k) = key {
        let mut cache = STAGE_CACHE.lock().unwrap();
        if let Some(entry) = cache.map.get(k).cloned() {
            cache.touch(k);
            return Some(entry);
        }
    }

    let ext = src_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let dst = tmp_dir.join(format!("tune-stage-{}.{ext}", uuid::Uuid::new_v4()));
    let resultat = match std::fs::copy(src_path, &dst) {
        Ok(bytes) => {
            // info!, pas debug! : cette copie retarde la première note du
            // fichier ENTIER. Invisible, elle a fait chercher les « lenteurs
            // au chargement » partout ailleurs (chantier du 24/08).
            tracing::info!(src = %src, staged = %dst.display(), bytes, "decode_source_staged_locally");
            let entry = Arc::new(StagedFile { path: dst, bytes });
            if let Some(k) = key.clone() {
                STAGE_CACHE.lock().unwrap().insert(k, entry.clone());
            }
            Some(entry)
        }
        Err(e) => {
            tracing::warn!(src = %src, error = %e, "decode_source_stage_failed_decoding_in_place");
            None
        }
    };

    // Libère le verrou single-flight de cette clé (le résultat vit dans le
    // cache ; garder un inflight orphelin fuirait de la mémoire).
    if let Some(k) = key {
        STAGE_CACHE.lock().unwrap().inflight.remove(&k);
    }
    resultat
}

/// Copie `src` → `dst` par blocs en publiant la progression sur `growth`, pour
/// que le décodeur lise au fur et à mesure. `finish()` à la fin, `fail()` sur
/// erreur (le lecteur remonte alors une erreur au lieu d'un EOF muet). Le temp
/// est supprimé par le `Drop` du `StagedFile` que tient le décodeur ; sur Unix
/// l'inode survit tant que ce copieur écrit, même si le chemin est délié.
#[cfg(unix)]
fn copier_en_fond(
    src: &str,
    dst: &std::path::Path,
    growth: &Arc<crate::audio::staged_growth::StageGrowth>,
) {
    use std::io::{Read, Write};
    let (mut r, mut w) = match (std::fs::File::open(src), std::fs::File::create(dst)) {
        (Ok(r), Ok(w)) => (r, w),
        _ => {
            growth.fail();
            return;
        }
    };
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    let mut total: u64 = 0;
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if w.write_all(&buf[..n]).is_err() || w.flush().is_err() {
                    growth.fail();
                    return;
                }
                total += n as u64;
                growth.advance(total);
            }
            Err(_) => {
                growth.fail();
                return;
            }
        }
    }
    growth.finish();
}

/// Date de modification en secondes epoch, 0 si indisponible — juste une
/// composante d'identité pour le cache, jamais une décision de lecture.
fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn stage_locally_for_decode(_src: &str) -> Option<Arc<StagedFile>> {
    None
}

/// Décoder un fichier en PCM entier.
///
/// Quand une cadence ou un nombre de canaux cible est fourni, la valeur est une
/// garantie sur le payload rendu, pas une préférence d'étiquetage. Chaque codec
/// décode d'abord honnêtement à son format natif (le DSD peut directement viser
/// la cadence demandée), puis une adaptation commune remixe et rééchantillonne.
/// Métadonnées, entrelacement et compte de trames décrivent ainsi les mêmes
/// octets (#1498, #2230).
pub fn decode_to_pcm(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    if target_sample_rate == Some(0) {
        return Err("requested PCM sample rate must be greater than zero".into());
    }
    if let Some(channels) = target_channels {
        checked_channels(channels, "requested PCM format")?;
    }
    // Même contrat que le scanner : un fichier exclu du catalogue ne peut pas
    // redevenir implicitement « transcodable » sur un autre chemin de lecture.
    // Pour DFF, l'en-tête distingue ici DSD brut de DST compressé.
    reject_unsupported_library_audio(file_path)?;
    // Stage network/external sources to a fast local temp before decoding so the
    // decoder's many small seeks don't each cost a network round-trip (Yves: NAS
    // over WiFi, 90s+ per track). No-op for local files. The guard lives for the
    // whole decode; the temp is removed when it drops.
    let _staged = stage_locally_for_decode(file_path);
    let file_path: &str = _staged
        .as_ref()
        .and_then(|s| s.path.to_str())
        .unwrap_or(file_path);

    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let decoded = if ext == "aiff" || ext == "aif" {
        super::aiff::decode_aiff_to_pcm(file_path, seek_s, max_duration_s)
    } else if ext == "dsf" || ext == "dff" {
        decode_dsd_to_pcm(
            file_path,
            &ext,
            target_sample_rate,
            None,
            seek_s,
            max_duration_s,
        )
    } else if is_wavpack(file_path) {
        // WavPack's decoder returns native samples. Never let it stamp target
        // metadata before the shared adapter has transformed the payload.
        super::wavpack::decode_wavpack_to_pcm(file_path, None, None, seek_s, max_duration_s)
    } else if ext == "ape" {
        // SPIKE (#1145): real Monkey's Audio playback via the pure-Rust
        // `ape-decoder` crate (MIT/Apache-2.0), replacing the broken in-tree
        // stub.
        //
        // catch_unwind : le catch symphonia plus bas ne couvrait PAS cette
        // branche (return anticipé) — un panic du décodeur entropique tiers
        // sur un fichier corrompu tuait le worker au lieu de produire une
        // erreur propre (durcissement, revue 2026-08-15).
        let fp = file_path.to_string();
        catch_unwind(AssertUnwindSafe(move || {
            decode_ape_to_pcm(&fp, seek_s, max_duration_s)
        }))
        .unwrap_or_else(|_| Err("ape: decoder panicked (corrupt file?)".into()))
    // Ogg-Vorbis / Ogg-FLAC (.ogg / .oga) is decoded NATIVELY by symphonia (the
    // "ogg" demuxer + "vorbis"/"flac" codec features are enabled). Il n'existe
    // plus aucun repli externe : tout ce qui n'est pas décodé ici l'est par
    // symphonia ou par libopus, ou ne l'est pas du tout.
    //
    // Opus needs libopus (symphonia has no Opus codec). We route the explicit
    // Opus/WebM extensions here, and ALSO sniff `.ogg`/`.oga` for OpusHead —
    // a `.ogg` file can carry Opus, and misrouting it to the Vorbis decoder
    // would fail (silence + 2 s loop). Vorbis/FLAC-in-Ogg fall through to
    // symphonia below.
    } else if ext == "opus"
        || ext == "webm"
        || ext == "weba"
        || ((ext == "ogg" || ext == "oga") && ogg_stream_is_opus(file_path))
    {
        // symphonia demuxes the container (mkv/ogg features) but has no Opus
        // decoder, so the packets are fed to libopus via audiopus. This gives
        // native Opus playback and restores YouTube→DLNA sound (forum #940).
        decode_opus_to_pcm(file_path, None, None, seek_s, max_duration_s)
    } else {
        // Wrap symphonia decode in catch_unwind — an unsupported codec or
        // malformed file must never panic-crash the server.
        let fp = file_path.to_string();
        let result = catch_unwind(AssertUnwindSafe(move || {
            decode_symphonia(&fp, None, None, seek_s, max_duration_s)
        }));
        match result {
            Ok(inner) => inner,
            Err(panic_info) => {
                let msg = panic_payload_to_string(&panic_info);
                error!(file = file_path, panic = %msg, "symphonia_decoder_panic");
                Err(format!("decode panic ({ext}): {msg}"))
            }
        }
    }?;

    adapt_decoded_audio(decoded, target_sample_rate, target_channels)
}

/// Sniff whether an Ogg stream carries Opus (`OpusHead`) rather than Vorbis.
///
/// A `.ogg`/`.oga` file can hold Vorbis OR Opus. Vorbis (and FLAC-in-Ogg) is
/// decoded natively by symphonia, but Opus-in-Ogg needs libopus. We route on
/// the actual codec, not the extension: the first Ogg page of an Opus stream
/// begins with the `OpusHead` magic (Vorbis begins with `\x01vorbis`,
/// FLAC-in-Ogg with `FLAC`). Reading the first 4 KiB is enough to see the id
/// header. Returns `false` on any I/O error — the caller then falls back to
/// symphonia, the correct behaviour for Vorbis/FLAC-in-Ogg.
fn ogg_stream_is_opus(file_path: &str) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    let Ok(mut f) = File::open(file_path) else {
        return false;
    };
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].windows(8).any(|w| w == b"OpusHead")
}

/// Decode Opus audio (Ogg-Opus / .opus, or Opus-in-WebM from YouTube) to PCM.
///
/// symphonia demuxes the WebM/Ogg container (the `mkv`/`ogg` features) but has
/// no Opus codec, so we pull the raw Opus packets and decode them with libopus
/// (audiopus). Opus is always 48 kHz internally; we return native 48 kHz / 16
/// bit and let the caller's encoder follow that rate. This gives native .opus /
/// Ogg-Opus playback and restores YouTube→DLNA audio (forum #940 and the
/// Opus/Ogg-Vorbis support request): before this, Opus streams decoded to
/// silence — and, for local files, looped every ~2 s because the silent decode
/// failure was mistaken for EOF.
///
/// ## Seek
///
/// Opus has no sample-accurate native seek. We seek the Ogg container coarsely
/// (page granularity) to land near the target, then skip the residual samples
/// using each packet's presentation timestamp (`pts`, in the track timebase,
/// which for Opus is 1/48000 s) so the returned buffer starts exactly at
/// `seek_s`. `max_duration_s` bounds the returned window measured *from* the
/// seek point.
fn decode_opus_to_pcm(
    file_path: &str,
    _target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    use audiopus::{
        Channels, MutSignals, SampleRate, coder::Decoder as OpusDecoder,
        packet::Packet as OpusPacket,
    };

    let file = File::open(file_path).map_err(|e| format!("open opus: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("opus probe: {e}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("opus: no audio track")?;
    let mut track_id = track.id;
    // Timebase to map packet.pts → sample index @ 48 kHz. Opus is always
    // 1/48000, but honour the container's declared timebase if present.
    let mut time_base = track.time_base;
    let ch: usize = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => {
            p.channels.as_ref().map(|c| c.count() as usize).unwrap_or(2)
        }
        _ => 2,
    }
    .clamp(1, 2); // libopus decoder: mono or stereo
    let channels_enum = if ch == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    };

    // Coarse container seek to land near the target page before sample-accurate
    // skipping. Best-effort: on failure we still skip from the start via pts.
    let mut skip_frames: i64 = if seek_s > 0.0 {
        let seconds = seek_s as i64;
        let nanos = ((seek_s - seconds as f64) * 1_000_000_000.0) as u32;
        if let Some(time) = Time::try_new(seconds, nanos) {
            let _ = format.seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time,
                    track_id: Some(track_id),
                },
            );
        }
        (seek_s * 48000.0) as i64
    } else {
        0
    };

    let mut decoder = OpusDecoder::new(SampleRate::Hz48000, channels_enum)
        .map_err(|e| format!("opus decoder init: {e}"))?;

    // 120 ms is the largest Opus frame @ 48 kHz (5760 samples/channel).
    let mut out_buf = vec![0i16; 5760 * ch];
    let mut samples_i32: Vec<i32> = Vec::new();
    // `as usize` saturates the f64 (e.g. `f64::MAX` from the converter's
    // "decode everything" call); the multiply must saturate too or debug
    // builds panic on overflow.
    let max_samples = if max_duration_s > 0.0 {
        ((max_duration_s * 48000.0) as usize).saturating_mul(ch)
    } else {
        usize::MAX
    };

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => {
                // Chained Ogg-Opus boundary (icecast rip, concatenated files).
                // Symphonia's OggReader starts a new physical stream and
                // returns `ResetRequired`; treating it as EOF truncated the
                // track to its first link — the output signalled a natural end
                // a few seconds in and the poller replayed the head of the
                // track over and over (#1270, « boucle de 2-3 s »). The
                // Vorbis/FLAC-in-Ogg path got this guard in #1632
                // (`rebuild_decoder_after_ogg_chain_reset`); this is the same
                // guard specialised for libopus: re-fetch the track, rebuild
                // the decoder, keep pulling packets.
                let Some(track) = format.default_track(TrackType::Audio) else {
                    break;
                };
                let new_ch = match &track.codec_params {
                    Some(CodecParameters::Audio(p)) => {
                        p.channels.as_ref().map(|c| c.count() as usize).unwrap_or(2)
                    }
                    _ => 2,
                }
                .clamp(1, 2);
                if new_ch != ch {
                    // The PCM contract (channel count) is already announced —
                    // a mid-stream layout change cannot be represented. Stop
                    // at the boundary, which is the pre-fix behaviour.
                    tracing::warn!(
                        file = file_path,
                        expect_channels = ch,
                        channels = new_ch,
                        "ogg_opus_chain_params_changed_stopping_at_boundary"
                    );
                    break;
                }
                let Ok(dec) = OpusDecoder::new(SampleRate::Hz48000, channels_enum) else {
                    break;
                };
                decoder = dec;
                track_id = track.id;
                time_base = track.time_base;
                // The seek point was consumed in the first link and pts
                // restarts at 0 in the new physical stream — never re-skip,
                // or the head of every following link would be dropped.
                skip_frames = 0;
                debug!(file = file_path, track_id, "ogg_opus_chain_decoder_rebuilt");
                continue;
            }
            Err(_) => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        if packet.data.is_empty() {
            continue;
        }
        // Sample index (@48 kHz) of this packet's first frame.
        let pkt_start_frame: i64 = match time_base {
            Some(tb) => {
                let t = tb.calc_time_saturating(packet.pts);
                (t.as_secs_f64() * 48000.0).round() as i64
            }
            None => packet.pts.get(),
        };
        let opus_pkt = match OpusPacket::try_from(&packet.data[..]) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let sig = match MutSignals::try_from(&mut out_buf[..]) {
            Ok(s) => s,
            Err(e) => return Err(format!("opus output buffer: {e}")),
        };
        let n = match decoder.decode(Some(opus_pkt), sig, false) {
            Ok(n) => n,
            Err(_) => continue,
        };
        // Per-channel frame count decoded from this packet.
        let n = n.min(out_buf.len() / ch);
        // Drop leading frames until we reach the requested seek point.
        let local_offset: usize = if skip_frames > pkt_start_frame {
            ((skip_frames - pkt_start_frame) as usize).min(n)
        } else {
            0
        };
        if local_offset < n {
            let start = local_offset * ch;
            let end = n * ch;
            samples_i32.extend(out_buf[start..end].iter().map(|&s| s as i32));
        }
        if samples_i32.len() >= max_samples {
            samples_i32.truncate(max_samples);
            break;
        }
    }

    if samples_i32.is_empty() {
        return Err("opus: decoded no audio".into());
    }
    let duration_s = (samples_i32.len() / ch) as f64 / 48000.0;
    Ok(DecodedAudio {
        samples_i32,
        bit_depth: 16,
        sample_rate: 48000,
        channels: ch as u32,
        duration_s,
    })
}

/// Streaming decode: decodes a file packet-by-packet and sends PCM chunks
/// progressively through the provided channel. This allows the HTTP stream
/// handler to start serving data to the DLNA renderer immediately, without
/// waiting for the entire file to be decoded.
///
/// Returns the emitted bit depth and sample rate on success. Requested rate and
/// channel count are guarantees on every emitted chunk.
/// For non-symphonia formats (AIFF, DSD, WavPack, APE), falls back to full
/// decode + chunked send (still benefits from the early session creation).
pub fn decode_to_pcm_streaming(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
) -> Result<(u16, u32), String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        None,
        tx,
        chunk_size,
        None,
        None,
        0.0,
        None,
    )
}

pub fn decode_to_pcm_streaming_with_notify(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
) -> Result<(u16, u32), String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        None,
        tx,
        chunk_size,
        Some(data_ready),
        None,
        0.0,
        None,
    )
}

pub fn decode_to_pcm_streaming_with_levels(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    target_bit_depth: Option<u16>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    levels_tx: tokio::sync::mpsc::UnboundedSender<super::tap::RawWindow>,
) -> Result<(u16, u32), String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        target_bit_depth,
        tx,
        chunk_size,
        Some(data_ready),
        Some(levels_tx),
        0.0,
        None,
    )
}

pub fn decode_to_pcm_streaming_seeked(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    target_bit_depth: Option<u16>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    levels_tx: tokio::sync::mpsc::UnboundedSender<super::tap::RawWindow>,
    seek_s: f64,
) -> Result<(u16, u32), String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        target_bit_depth,
        tx,
        chunk_size,
        Some(data_ready),
        Some(levels_tx),
        seek_s,
        None,
    )
}

/// Variante HTTP seekable du decodeur progressif. La source a deja prouve le
/// support de `Range`; Symphonia peut donc lire l'atome `moov` a la fin d'un
/// M4A puis revenir aux premiers paquets sans telecharger tout le media (#1885).
pub fn decode_http_range_to_pcm_streaming_seeked(
    source: super::http_range::HttpRangeSource,
    codec_hint: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    target_bit_depth: Option<u16>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    levels_tx: tokio::sync::mpsc::UnboundedSender<super::tap::RawWindow>,
    seek_s: f64,
) -> Result<(u16, u32), String> {
    let source_name = format!("flux-distant.{codec_hint}");
    decode_to_pcm_streaming_inner(
        &source_name,
        target_sample_rate,
        target_channels,
        target_bit_depth,
        tx,
        chunk_size,
        Some(data_ready),
        Some(levels_tx),
        seek_s,
        Some(Box::new(source)),
    )
}

fn decode_to_pcm_streaming_inner(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    target_bit_depth: Option<u16>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: Option<std::sync::Arc<tokio::sync::Notify>>,
    levels_tx: Option<tokio::sync::mpsc::UnboundedSender<super::tap::RawWindow>>,
    seek_s: f64,
    source_override: Option<Box<dyn MediaSource>>,
) -> Result<(u16, u32), String> {
    if target_sample_rate == Some(0) {
        return Err("stream target sample rate must be greater than zero".into());
    }
    if let Some(channels) = target_channels {
        checked_channels(channels, "stream target")?;
    }
    if let Some(bit_depth) = target_bit_depth
        && !matches!(bit_depth, 16 | 24 | 32)
    {
        return Err(format!(
            "stream target bit depth {bit_depth} is unsupported (expected 16, 24 or 32)"
        ));
    }
    reject_unsupported_library_audio(file_path)?;
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mut first_chunk_sent = false;
    // DSD files (DSF/DFF): streaming decode using chunk-based DSD→PCM converter.
    // This avoids loading the entire DSD file into memory (200MB+ → OOM).
    if matches!(ext.as_str(), "dsf" | "dff") {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for streaming decode")?;
        let output_bd: u16 = target_bit_depth.unwrap_or(24);

        // Determine output rate + channels before sending WAV header
        let (dsd_rate, dsd_ch) = if ext == "dsf" {
            let info = super::dsf::parse_dsf(file_path)?;
            (info.sample_rate, info.channels)
        } else {
            let info = super::dff::parse_dff(file_path)?;
            (info.sample_rate, info.channels as u32)
        };
        let dsd_output_rate =
            target_sample_rate.unwrap_or_else(|| super::dsd_to_pcm::choose_output_rate(dsd_rate));
        let dsd_channels =
            checked_channels(target_channels.unwrap_or(dsd_ch), "DSD stream target")?;

        // Send WAV header so LocalOutput can parse stream metadata
        if target_bit_depth.is_some() {
            let wav_hdr = super::wav::build_wav_header(dsd_channels, dsd_output_rate, output_bd);
            if let Err(_) = rt.block_on(tx.send(wav_hdr.to_vec())) {
                return Ok((output_bd, dsd_output_rate));
            }
            if let Some(n) = &data_ready {
                n.notify_one();
            }
            first_chunk_sent = true;
            debug!(
                source_rate = dsd_output_rate,
                output_bd,
                channels = dsd_channels,
                "streaming_decode_wav_header_sent_dsd"
            );
        }

        return decode_dsd_streaming(
            file_path,
            &ext,
            target_sample_rate,
            target_channels,
            output_bd,
            tx,
            chunk_size,
            &mut first_chunk_sent,
            &data_ready,
            &levels_tx,
            &rt,
            seek_s,
        );
    }

    // Opus (native .opus / Ogg-Opus, or Opus-in-WebM) has no symphonia codec —
    // it is decoded with libopus (audiopus) via decode_to_pcm, then streamed as
    // chunks. Before this the streaming path fed Opus to symphonia's
    // make_audio_decoder, which failed → the stream produced no audio and (for
    // local files) looped every ~2 s. `.ogg`/`.oga` is sniffed: OpusHead → here,
    // Vorbis/FLAC-in-Ogg → the symphonia streaming path below.
    if matches!(ext.as_str(), "opus" | "webm" | "weba")
        || ((ext == "ogg" || ext == "oga") && ogg_stream_is_opus(file_path))
    {
        // Full decode (seek honoured) with the same exact target contract.
        let decoded = decode_to_pcm(file_path, target_sample_rate, target_channels, seek_s, 0.0)?;
        let output_bd = target_bit_depth.unwrap_or(decoded.bit_depth);
        let pcm_bytes = if output_bd != decoded.bit_depth {
            convert_pcm_bit_depth(&decoded.samples_i32, decoded.bit_depth, output_bd)
        } else {
            decoded.pcm_bytes()
        };
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for streaming decode")?;
        let ch = decoded.channels as u16;
        let sr = decoded.sample_rate;
        if target_bit_depth.is_some() {
            let wav_hdr = super::wav::build_wav_header(ch, sr, output_bd);
            if rt.block_on(tx.send(wav_hdr.to_vec())).is_err() {
                return Ok((output_bd, sr));
            }
            if let Some(ref n) = data_ready {
                n.notify_one();
            }
            first_chunk_sent = true;
            debug!(
                source_rate = sr,
                output_bd,
                channels = ch,
                format = ext.as_str(),
                "streaming_decode_wav_header_sent_opus"
            );
        }
        let batch = frame_aligned_chunk_len(chunk_size, output_bd, ch);
        for chunk in pcm_bytes.chunks(batch) {
            match rt.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                tx.send(chunk.to_vec()),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("streaming_decode_consumer_dropped (opus)");
                    return Ok((output_bd, sr));
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = SEND_TIMEOUT_SECS,
                        "streaming_decode_send_timeout (opus)"
                    );
                    return Ok((output_bd, sr));
                }
            }
            if !first_chunk_sent {
                first_chunk_sent = true;
                if let Some(ref n) = data_ready {
                    n.notify_one();
                }
            }
            if let Some(ref ltx) = levels_tx {
                super::tap::send_windowed_pcm(ltx, chunk, output_bd, ch, sr);
            }
        }
        return Ok((output_bd, sr));
    }

    // Non-symphonia formats: fall back to full decode then stream chunks.
    // This still benefits from the session being created early.
    if matches!(ext.as_str(), "aiff" | "aif" | "wv" | "ape") {
        let decoded = decode_to_pcm(file_path, target_sample_rate, target_channels, 0.0, 0.0)?;
        // Use target_bit_depth if provided, otherwise use the decoder's native depth.
        // This ensures the PCM byte encoding matches the WAV header declaration.
        let output_bd = target_bit_depth.unwrap_or(decoded.bit_depth);
        let pcm_bytes = if output_bd != decoded.bit_depth {
            convert_pcm_bit_depth(&decoded.samples_i32, decoded.bit_depth, output_bd)
        } else {
            decoded.pcm_bytes()
        };
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for streaming decode")?;
        // The shared batch adapter has already made decoded metadata and bytes
        // agree, including for AIFF/APE/WavPack.
        let ch = decoded.channels as u16;
        let sr = decoded.sample_rate;
        if target_bit_depth.is_some() {
            let wav_hdr = super::wav::build_wav_header(ch, sr, output_bd);
            if let Err(_) = rt.block_on(tx.send(wav_hdr.to_vec())) {
                return Ok((output_bd, sr));
            }
            if let Some(ref n) = data_ready {
                n.notify_one();
            }
            first_chunk_sent = true;
            debug!(
                source_rate = sr,
                output_bd,
                channels = ch,
                format = ext.as_str(),
                "streaming_decode_wav_header_sent_fallback"
            );
        }
        let batch = frame_aligned_chunk_len(chunk_size, output_bd, ch);
        for chunk in pcm_bytes.chunks(batch) {
            // Send PCM data first, compute levels after (same rationale
            // as the symphonia path: avoid delaying the audio stream).
            match rt.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                tx.send(chunk.to_vec()),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("streaming_decode_consumer_dropped (fallback)");
                    return Ok((output_bd, sr));
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = SEND_TIMEOUT_SECS,
                        "streaming_decode_send_timeout (fallback)"
                    );
                    return Ok((output_bd, sr));
                }
            }
            if !first_chunk_sent {
                first_chunk_sent = true;
                if let Some(ref n) = data_ready {
                    n.notify_one();
                }
            }
            if let Some(ref ltx) = levels_tx {
                super::tap::send_windowed_pcm(ltx, chunk, output_bd, ch, sr);
            }
        }
        return Ok((output_bd, sr));
    }

    // Symphonia streaming decode: packet-by-packet progressive output
    let mss = if let Some(source) = source_override {
        MediaSourceStream::new(source, Default::default())
    } else if let Some(growth) = crate::audio::staged_growth::take_for(file_path) {
        let source = crate::audio::staged_growth::SeekableGrowingSource::open(file_path, growth)
            .map_err(|e| format!("open (staged growing): {e}"))?;
        MediaSourceStream::new(Box::new(source), Default::default())
    } else if let Some(growth) = crate::audio::dash_growth::take_for(file_path) {
        let source = crate::audio::dash_growth::GrowingFileSource::open(file_path, growth)
            .map_err(|e| format!("open (growing): {e}"))?;
        MediaSourceStream::new(Box::new(source), Default::default())
    } else {
        let file = File::open(file_path).map_err(|e| format!("open: {e}"))?;
        MediaSourceStream::new(Box::new(file), Default::default())
    };

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format: Box<dyn FormatReader> = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no default audio track")?;

    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("track has no audio codec parameters".into()),
    };
    let mut track_id = track.id;
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    // Borné au conteneur PCM : c'est cette valeur qui sert AUSSI de `shift`
    // de droitisation et de `from_bd` de conversion, les trois doivent donc
    // désigner la même largeur d'octets (#2157).
    let source_bd = container_bit_depth(resolve_bit_depth(&audio_params));
    let shift = 32u16.saturating_sub(source_bd);

    // Use target_bit_depth if provided, otherwise use the source's native depth.
    // This ensures the PCM byte encoding matches the WAV header declaration.
    let output_bd = target_bit_depth.unwrap_or(source_bd);
    if output_bd != source_bd {
        debug!(
            source_bd,
            output_bd,
            file = file_path,
            "streaming_decode_bit_depth_conversion"
        );
    }
    let output_rate = target_sample_rate.unwrap_or(source_rate);
    let output_channels = target_channels.unwrap_or_else(|| source_channels);
    let mut pcm_adapter = StreamingPcmAdapter::new(
        source_bd,
        source_channels,
        output_channels,
        source_rate,
        output_rate,
    )?;

    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "no tokio runtime for streaming decode")?;

    // Sample-accurate seek: how many leading frames to discard AFTER the
    // demuxer seek so playback starts EXACTLY at the requested position.
    // `SeekMode::Accurate` lands on the packet boundary at-or-before the
    // target (actual_ts <= required_ts); the residual (required_ts - actual_ts)
    // frames are trimmed in the decode loop below. `Coarse` (the old mode) did
    // no trim and could land a full seek-index granule off — inaudible on local
    // FLAC (dense native seektable) but seconds off on freshly transcoded Qobuz
    // FLAC (sparse/absent seektable) and fragmented Tidal DASH, so streaming
    // seeks appeared to overshoot the clicked position (DEvir, v0.9.50, ASIO).
    let mut frames_to_skip: u64 = 0;
    if seek_s > 0.0 {
        let seconds = seek_s as i64;
        let nanos = ((seek_s - seconds as f64) * 1_000_000_000.0) as u32;
        let time = Time::try_new(seconds, nanos).unwrap_or(Time::ZERO);
        match format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        ) {
            Ok(seeked) => {
                // Symphonia 0.6 `Timestamp` is a newtype over i64; `.get()`
                // yields the raw frame index. Accurate seek guarantees
                // actual <= required, but clamp defensively.
                let required = seeked.required_ts.get();
                let actual = seeked.actual_ts.get();
                frames_to_skip = (required - actual).max(0) as u64;
                // Clear the decoder's internal state so the first post-seek
                // packet decodes cleanly (Symphonia requires a reset after seek).
                decoder.reset();
                debug!(
                    file = file_path,
                    seek_s,
                    required_ts = required,
                    actual_ts = actual,
                    frames_to_skip,
                    "streaming_decode_seeked"
                );
            }
            Err(e) => {
                debug!(file = file_path, seek_s, error = %e, "streaming_decode_seek_failed");
            }
        }
    }

    // The header describes the adapted payload, whose source format was read
    // from Symphonia rather than trusted from API metadata.
    if target_bit_depth.is_some() {
        let channels = checked_channels(output_channels, "stream target")?;
        let wav_hdr = super::wav::build_wav_header(channels, output_rate, output_bd);
        if let Err(_) = rt.block_on(tx.send(wav_hdr.to_vec())) {
            return Ok((output_bd, output_rate));
        }
        if let Some(ref n) = data_ready {
            n.notify_one();
        }
        first_chunk_sent = true;
        debug!(
            source_rate,
            output_rate, output_bd, channels, "streaming_decode_wav_header_sent"
        );
    }

    // Accumulate PCM bytes and flush when exceeding chunk_size.
    // This avoids sending tiny per-packet buffers over the channel.
    // Drain size is frame-aligned so 24-bit stereo VU stays correct
    // (fixed 32768 % 6 == 2 would shift sample boundaries after the 1st batch).
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(chunk_size * 2);
    let output_channels_u16 = checked_channels(output_channels, "stream target")?;
    let flush_len = frame_aligned_chunk_len(chunk_size, output_bd, output_channels_u16);
    let mut total_samples: usize = 0;
    let mut source_samples_seen: usize = 0;
    let mut decode_errors: usize = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                debug!(file = file_path, total_samples, "streaming_decode_eof");
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                // Chained Ogg boundary — rebuild the decoder and keep going
                // instead of truncating the track at the first link (#1270).
                match rebuild_decoder_after_ogg_chain_reset(
                    format.as_ref(),
                    file_path,
                    source_rate,
                    source_channels,
                ) {
                    Some((id, dec)) => {
                        track_id = id;
                        decoder = dec;
                        continue;
                    }
                    None => break,
                }
            }
            Err(e) => {
                tracing::warn!(file = file_path, error = %e, total_samples, source_bd, "streaming_decode_packet_error");
                break;
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                decode_errors += 1;
                if decode_errors <= 3 {
                    tracing::warn!(file = file_path, error = %e, total_samples, source_bd, "streaming_decode_frame_error");
                }
                continue;
            }
        };

        let mut packet_samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut packet_samples);

        // Trim the leading frames left over from the Accurate seek so the very
        // first emitted sample is the requested position (see `frames_to_skip`
        // above). `packet_samples` is interleaved (channels consecutive), so a
        // frame == `source_channels` samples; both counts are frame-aligned.
        if frames_to_skip > 0 {
            let ch = source_channels.max(1) as usize;
            let skip_samples = (frames_to_skip as usize)
                .saturating_mul(ch)
                .min(packet_samples.len());
            packet_samples.drain(..skip_samples);
            frames_to_skip -= (skip_samples / ch) as u64;
            if packet_samples.is_empty() {
                continue;
            }
        }

        // Normalize: right-justify samples (same as batch decode)
        if shift > 0 && shift < 32 {
            for s in packet_samples.iter_mut() {
                *s >>= shift;
            }
        }

        source_samples_seen += packet_samples.len();
        let output_samples = pcm_adapter.push(&packet_samples)?;
        total_samples += output_samples.len();
        append_pcm_samples(&mut pcm_buf, &output_samples, source_bd, output_bd);

        while pcm_buf.len() >= flush_len {
            let chunk: Vec<u8> = pcm_buf.drain(..flush_len).collect();
            // Send PCM data FIRST to avoid delaying the audio stream.
            // compute_levels() is CPU-intensive (iterates all frames with
            // floating-point math) and was previously called before send(),
            // introducing micro-pauses that caused Squeezebox/LMS stuttering.
            match rt.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                tx.send(chunk.clone()),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("streaming_decode_consumer_dropped");
                    return Ok((output_bd, output_rate));
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = SEND_TIMEOUT_SECS,
                        "streaming_decode_send_timeout"
                    );
                    return Ok((output_bd, output_rate));
                }
            }
            if !first_chunk_sent {
                first_chunk_sent = true;
                if let Some(ref n) = data_ready {
                    n.notify_one();
                }
            }
            // Compute and send audio levels AFTER the PCM chunk is dispatched.
            // The unbounded channel never blocks; the clone above is cheap
            // compared to the latency savings for network outputs.
            if let Some(ref ltx) = levels_tx {
                super::tap::send_windowed_pcm(
                    ltx,
                    &chunk,
                    output_bd,
                    output_channels_u16,
                    output_rate,
                );
            }
        }
    }

    let tail = if source_samples_seen > 0 {
        pcm_adapter.finish()?
    } else {
        Vec::new()
    };
    total_samples += tail.len();
    append_pcm_samples(&mut pcm_buf, &tail, source_bd, output_bd);

    // Flush remaining bytes
    if !pcm_buf.is_empty() {
        match rt.block_on(tokio::time::timeout(
            std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
            tx.send(pcm_buf),
        )) {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                debug!("streaming_decode_consumer_dropped (final)");
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = SEND_TIMEOUT_SECS,
                    "streaming_decode_send_timeout (final)"
                );
            }
        }
    }

    let total_frames = total_samples as f64 / output_channels_u16 as f64;
    let duration_s = total_frames / output_rate as f64;

    // If seek was beyond EOF (0 samples decoded), send a short silence
    // to prevent empty stream that crashes exclusive ASIO readers.
    if source_samples_seen == 0 && seek_s > 0.0 {
        let ch = output_channels_u16 as usize;
        let silence_frames = (output_rate as usize) / 10; // 100ms
        let silence = vec![0u8; silence_frames * ch * (output_bd as usize / 8)];
        let _ = rt.block_on(tx.send(silence));
        tracing::warn!(file = file_path, seek_s, "seek_beyond_eof_sent_silence");
    }

    debug!(
        file = file_path,
        samples = total_samples,
        source_rate,
        rate = output_rate,
        source_channels,
        channels = output_channels,
        source_bd,
        output_bd,
        duration_s,
        "decoded_symphonia_streaming"
    );

    Ok((output_bd, output_rate))
}

/// Extract a human-readable message from a panic payload.
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// SPIKE (#1145): decode a Monkey's Audio (.ape) file to PCM using the
/// pure-Rust `ape-decoder` crate. Returns right-justified i32 samples in a
/// `DecodedAudio`, matching the symphonia path's contract.
///
/// The crate decodes to interleaved little-endian PCM bytes at the file's
/// native bit depth (16/24/32). We deinterleave into right-justified i32
/// samples so `pcm_bytes()` / `convert_pcm_bit_depth()` behave exactly as for
/// the symphonia decoders. `seek_s` uses the crate's sample-accurate
/// `decode_from`; `max_duration_s` truncates the sample buffer afterward.
///
/// NOT production-hardened: no local staging tuning, decodes the whole file
/// into memory (a large 24/96 .ape can be ~1 GB PCM — the streaming path in
/// decode_to_pcm_streaming_inner already routes .ape through full decode + chunk).
fn decode_ape_to_pcm(
    file_path: &str,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    use std::io::BufReader;

    let file = File::open(file_path).map_err(|e| format!("open ape: {e}"))?;
    let mut decoder =
        ape_decoder::ApeDecoder::new(BufReader::new(file)).map_err(|e| format!("ape open: {e}"))?;

    // Copy out the fields we need BEFORE any &mut decode call: info() borrows
    // the decoder immutably and decode_all/decode_from borrow it mutably.
    let (sample_rate, channels, bit_depth, is_float, is_signed_8bit, total_samples) = {
        let info = decoder.info();
        (
            info.sample_rate,
            info.channels as u32,
            info.bits_per_sample,
            info.is_floating_point,
            info.is_signed_8bit,
            info.total_samples,
        )
    };

    if is_float {
        return Err("Monkey's Audio (.ape) floating-point source not supported".into());
    }
    if !matches!(bit_depth, 8 | 16 | 24 | 32) {
        return Err(format!("ape: unsupported bit depth {bit_depth}"));
    }
    // Garde-fous d'en-tête : un fichier corrompu/forgé peut annoncer des
    // valeurs absurdes que le décodage intégral en mémoire transformerait en
    // allocation démesurée (un 24/96 légitime fait déjà ~1 Go de PCM).
    if channels == 0 || channels > 8 {
        return Err(format!("ape: implausible channel count {channels}"));
    }
    if sample_rate == 0 || sample_rate > 384_000 {
        return Err(format!("ape: implausible sample rate {sample_rate}"));
    }
    // Plafond d'allocation calculé depuis l'en-tête, AVANT decode_all.
    const MAX_APE_PCM_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
    let bytes_per = u64::from(bit_depth / 8).max(1);
    let expected_bytes = total_samples
        .saturating_mul(u64::from(channels))
        .saturating_mul(bytes_per);
    if expected_bytes > MAX_APE_PCM_BYTES {
        return Err(format!(
            "ape: decoded size would exceed {} GiB (header claims {total_samples} samples)",
            MAX_APE_PCM_BYTES / (1024 * 1024 * 1024)
        ));
    }

    // Sample-accurate seek: decode from the requested sample offset onward.
    let start_sample = if seek_s > 0.0 {
        (seek_s * sample_rate as f64) as u64
    } else {
        0
    };

    let pcm: Vec<u8> = if start_sample > 0 {
        decoder
            .decode_from(start_sample)
            .map_err(|e| format!("ape decode_from: {e}"))?
    } else {
        decoder
            .decode_all()
            .map_err(|e| format!("ape decode_all: {e}"))?
    };

    // Deinterleave native-depth LE PCM bytes into right-justified i32 samples.
    let bytes_per_sample = (bit_depth / 8) as usize;
    if bytes_per_sample == 0 || pcm.len() % bytes_per_sample != 0 {
        return Err("ape: PCM byte length not aligned to sample size".into());
    }
    let mut samples: Vec<i32> = Vec::with_capacity(pcm.len() / bytes_per_sample);
    match bit_depth {
        8 => {
            // APE 8-bit is unsigned by default; is_signed_8bit overrides.
            let signed = is_signed_8bit;
            for b in &pcm {
                let v = if signed {
                    *b as i8 as i32
                } else {
                    *b as i32 - 128
                };
                samples.push(v);
            }
        }
        16 => {
            for b in pcm.chunks_exact(2) {
                samples.push(i16::from_le_bytes([b[0], b[1]]) as i32);
            }
        }
        24 => {
            for b in pcm.chunks_exact(3) {
                let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                // sign-extend 24-bit -> i32
                samples.push((v << 8) >> 8);
            }
        }
        32 => {
            for b in pcm.chunks_exact(4) {
                samples.push(i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            }
        }
        _ => unreachable!(),
    }

    // Optional truncation to max_duration_s (whole frames).
    if max_duration_s > 0.0 {
        let max_samples = (max_duration_s * sample_rate as f64 * channels as f64) as usize;
        if samples.len() > max_samples {
            samples.truncate(max_samples);
        }
    }

    // 8-bit is widened to 16-bit for the rest of the pipeline (WAV min depth).
    let out_bd = if bit_depth == 8 { 16 } else { bit_depth };
    if bit_depth == 8 {
        for s in samples.iter_mut() {
            *s <<= 8;
        }
    }

    let total_frames = samples.len() as f64 / channels as f64;
    let duration_s = total_frames / sample_rate as f64;

    debug!(
        file = file_path,
        samples = samples.len(),
        rate = sample_rate,
        channels,
        bit_depth = out_bd,
        duration_s,
        "decoded_ape (spike #1145)"
    );

    Ok(DecodedAudio {
        samples_i32: samples,
        bit_depth: out_bd,
        sample_rate,
        channels,
        duration_s,
    })
}

/// Remux a Tidal HI-RES DASH FLAC-in-fMP4 file into a native `.flac` file
/// WITHOUT decoding or re-encoding (#1146). The source is already FLAC (Tidal
/// delivers FLAC frames inside a fragmented MP4), so the old path — decode to
/// PCM then re-encode FLAC — wastes ~59s on a weak CPU (.18, HI-RES) for a
/// bit-identical result. Here we reuse symphonia's mp4 demuxer to pull the raw
/// FLAC frames (`packet.data`, unmodified) and the STREAMINFO (`extra_data`, the
/// 34-byte block body), and write `fLaC` + metadata + frames = a valid, bit-exact
/// `.flac` in a few hundred ms (I/O-bound copy). MD5 is preserved (frames copied
/// verbatim → decoded audio unchanged → original checksum still valid).
///
/// Only valid when the renderer takes FLAC and no zone EQ is active (an EQ would
/// mutate samples, which a remux cannot). Returns `(file_size, bit_depth,
/// sample_rate)`, matching the decode+encode path's tuple.
pub fn remux_flac_dash(input_path: &str, output_path: &str) -> Result<(u64, u16, u32), String> {
    let file = File::open(input_path).map_err(|e| format!("remux open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(input_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("remux probe: {e}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("remux: no default audio track")?;
    let track_id = track.id;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("remux: track has no audio codec parameters".into()),
    };
    // The mp4 `dfLa` box exposes the 34-byte METADATA_BLOCK_STREAMINFO body here.
    let stream_info = audio_params
        .extra_data
        .clone()
        .ok_or("remux: no FLAC STREAMINFO (extra_data) — not a FLAC-in-mp4 track")?;
    if stream_info.len() != 34 {
        return Err(format!(
            "remux: STREAMINFO is {} bytes, expected 34 — refusing to remux",
            stream_info.len()
        ));
    }
    let sample_rate = audio_params.sample_rate.unwrap_or(44100);
    let bit_depth = resolve_bit_depth(&audio_params);

    let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
    // "fLaC" stream marker.
    out.extend_from_slice(b"fLaC");
    // METADATA_BLOCK_HEADER: is_last=0, type=0 (STREAMINFO), 24-bit length = 34.
    out.push(0x00);
    out.extend_from_slice(&[0x00, 0x00, 0x22]);
    out.extend_from_slice(&stream_info);
    // Empty VORBIS_COMMENT (is_last=1, type=4, len=8): vendor_len=0 + count=0.
    // Some DLNA renderers reject a FLAC stream without a VORBIS_COMMENT block, so
    // append the same empty one the native encoder emits.
    out.push(0x84);
    out.extend_from_slice(&[0x00, 0x00, 0x08]);
    out.extend_from_slice(&[0u8; 8]);

    // Concatenate the raw FLAC frames in order — one mp4 sample = one FLAC frame.
    let mut frame_count: u64 = 0;
    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id == track_id {
                    out.extend_from_slice(&packet.data);
                    frame_count += 1;
                }
            }
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(format!("remux next_packet: {e}")),
        }
    }
    if frame_count == 0 {
        return Err("remux: no FLAC frames extracted".into());
    }

    std::fs::write(output_path, &out).map_err(|e| format!("remux write: {e}"))?;
    Ok((out.len() as u64, bit_depth, sample_rate))
}

/// Streaming variant of [`remux_flac_dash`] (#1146): remux a STILL-DOWNLOADING
/// DASH fMP4 and push the FLAC bytes into an mpsc channel AS the frames arrive,
/// so a chunked-capable renderer (Lavf/DMP-A8) starts playing almost immediately
/// — matching Qobuz's instant start — instead of waiting for the whole file to
/// download+remux. Reads via the growing-file source (dash_growth registry) so
/// `next_packet` blocks at the download frontier rather than hitting EOF.
///
/// Runs on a blocking thread (symphonia is sync) → uses `blocking_send`. Returns
/// when the stream is fully sent, or early (Ok) if the consumer is gone.
pub fn remux_flac_dash_stream(input_path: &str, tx: mpsc::Sender<Vec<u8>>) -> Result<(), String> {
    // Source: the growing fMP4 if a streaming download registered a handle, else
    // a plain file (fully downloaded).
    let mss = if let Some(growth) = crate::audio::dash_growth::take_for(input_path) {
        let src = crate::audio::dash_growth::GrowingFileSource::open(input_path, growth)
            .map_err(|e| format!("remux-stream open (growing): {e}"))?;
        MediaSourceStream::new(Box::new(src), Default::default())
    } else {
        let file = File::open(input_path).map_err(|e| format!("remux-stream open: {e}"))?;
        MediaSourceStream::new(Box::new(file), Default::default())
    };

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(input_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("remux-stream probe: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("remux-stream: no default audio track")?;
    let track_id = track.id;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("remux-stream: track has no audio codec parameters".into()),
    };
    let stream_info = audio_params
        .extra_data
        .clone()
        .ok_or("remux-stream: no FLAC STREAMINFO (extra_data)")?;
    if stream_info.len() != 34 {
        return Err(format!(
            "remux-stream: STREAMINFO is {} bytes, expected 34",
            stream_info.len()
        ));
    }

    // Header: fLaC + STREAMINFO + empty VORBIS_COMMENT.
    let mut header = Vec::with_capacity(64);
    header.extend_from_slice(b"fLaC");
    header.push(0x00);
    header.extend_from_slice(&[0x00, 0x00, 0x22]);
    header.extend_from_slice(&stream_info);
    header.push(0x84);
    header.extend_from_slice(&[0x00, 0x00, 0x08]);
    header.extend_from_slice(&[0u8; 8]);
    if tx.blocking_send(header).is_err() {
        return Ok(()); // consumer already gone
    }

    // Frames: batch ~64 KB before sending to keep channel churn low.
    const FLUSH: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(96 * 1024);
    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id == track_id {
                    buf.extend_from_slice(&packet.data);
                    if buf.len() >= FLUSH && tx.blocking_send(std::mem::take(&mut buf)).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(format!("remux-stream next_packet: {e}")),
        }
    }
    if !buf.is_empty() {
        let _ = tx.blocking_send(buf);
    }
    Ok(())
}

/// Symphonia-based decoder for standard formats (FLAC, MP3, WAV, M4A, OGG, etc).
fn decode_symphonia(
    file_path: &str,
    _target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    // Streaming DASH (#1146 Plan C step 2): if a background task is still
    // appending this fMP4, decode it as it grows via a blocking MediaSource
    // instead of a plain File (which would EOF-truncate at the write frontier).
    // Registry is empty unless TUNE_DASH_STREAM_DECODE armed a download — then
    // this is byte-identical to the File path.
    let mss = if let Some(growth) = crate::audio::staged_growth::take_for(file_path) {
        // Staging pipeliné (lenteurs Yves, phase 2) : le fichier réseau est
        // copié en fond ; on décode au fur et à mesure via une source SEEKABLE
        // (l'ALAC moov-at-end peut chercher la fin). Registre vide sauf
        // TUNE_STAGE_STREAM_DECODE — sinon octet pour octet identique au File.
        let src = crate::audio::staged_growth::SeekableGrowingSource::open(file_path, growth)
            .map_err(|e| format!("open (staged growing): {e}"))?;
        MediaSourceStream::new(Box::new(src), Default::default())
    } else if let Some(growth) = crate::audio::dash_growth::take_for(file_path) {
        let src = crate::audio::dash_growth::GrowingFileSource::open(file_path, growth)
            .map_err(|e| format!("open (growing): {e}"))?;
        MediaSourceStream::new(Box::new(src), Default::default())
    } else {
        let file = File::open(file_path).map_err(|e| format!("open: {e}"))?;
        MediaSourceStream::new(Box::new(file), Default::default())
    };

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format: Box<dyn FormatReader> = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no default audio track")?;

    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("track has no audio codec parameters".into()),
    };
    let mut track_id = track.id;
    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Borné au conteneur PCM : c'est cette valeur qui sert AUSSI de `shift`
    // de droitisation et de `from_bd` de conversion, les trois doivent donc
    // désigner la même largeur d'octets (#2157).
    let source_bd = container_bit_depth(resolve_bit_depth(&audio_params));

    // Seek if requested. On a non-seekable source (e.g. a FLAC over SMB with no
    // seektable) `format.seek` fails and the reader stays at position 0. If we
    // then kept decoding, we'd return the *first* segment again for every seek
    // offset — the ReplayGain / trailing-silence analyzers advance `seek` by a
    // full segment each time, never see a short final segment, and loop forever
    // (#1277, a regression of the #1109 segmented-decode OOM fix). Honour the
    // error by returning an EMPTY segment: the analyzers break on the resulting
    // `is_empty()`. The first segment (seek_s == 0.0, no seek) always decodes
    // normally, so the analysis still runs over the head of the track.
    if seek_s > 0.0 {
        let seconds = seek_s as i64;
        let nanos = ((seek_s - seconds as f64) * 1_000_000_000.0) as u32;
        let time = Time::try_new(seconds, nanos).unwrap_or(Time::ZERO);
        if format
            .seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time,
                    track_id: Some(track_id),
                },
            )
            .is_err()
        {
            debug!(
                file = file_path,
                seek_s, "decode_symphonia_seek_failed_returning_empty"
            );
            return Ok(DecodedAudio {
                samples_i32: Vec::new(),
                bit_depth: source_bd,
                sample_rate: source_rate,
                channels: source_channels,
                duration_s: 0.0,
            });
        }
    }

    let mut all_samples: Vec<i32> = Vec::new();
    let max_samples = if max_duration_s > 0.0 {
        (max_duration_s * source_rate as f64 * source_channels as f64) as usize
    } else {
        usize::MAX
    };

    loop {
        if all_samples.len() >= max_samples {
            break;
        }

        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                // Chained Ogg boundary — rebuild the decoder and keep going
                // instead of truncating the track at the first link (#1270).
                match rebuild_decoder_after_ogg_chain_reset(
                    format.as_ref(),
                    file_path,
                    source_rate,
                    source_channels,
                ) {
                    Some((id, dec)) => {
                        track_id = id;
                        decoder = dec;
                        continue;
                    }
                    None => break,
                }
            }
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut packet_samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut packet_samples);
        all_samples.extend_from_slice(&packet_samples);
    }

    if all_samples.len() > max_samples {
        all_samples.truncate(max_samples);
    }

    // Symphonia's copy_to_vec_interleaved::<i32>() returns samples
    // left-justified in the 32-bit range (e.g. a 16-bit sample is shifted
    // left by 16). Normalize to right-justified so that pcm_bytes() can
    // directly extract the correct byte width without further shifting.
    let shift = 32u16.saturating_sub(source_bd);
    if shift > 0 && shift < 32 {
        for s in all_samples.iter_mut() {
            *s >>= shift;
        }
    }

    // On rapporte ce qu'on a REELLEMENT decode, jamais ce qui a ete demande.
    //
    // Ce chemin ne reechantillonne pas et ne downmixe pas : les echantillons
    // restent entrelaces a la cadence source. Etiqueter la sortie avec les
    // valeurs DEMANDEES transformait une intention en fait, et trois appelants
    // s'y fiaient (JP Robbe, #1498) :
    //
    // - `embedding.rs` passait `d.channels` (donc 1) a `to_mono_f32`, qui
    //   prenait alors sa branche « deja mono » et ne moyennait rien. Le modele
    //   acoustique recevait du L/R/L/R a 44,1 kHz presente comme du mono
    //   48 kHz — la moitie de la duree utile, et un timbre brouille. C'est le
    //   defaut #1108, que son correctif n'a jamais pu corriger : il etait
    //   neutralise ici, en amont.
    // - `pipeline.rs` construisait l'encodeur avec l'etiquette tout en lui
    //   donnant les vrais echantillons : en-tete annoncant la cadence cible,
    //   donnees a la cadence source. Lecture a la mauvaise vitesse.
    // - `analyzer.rs` interpretait les segments avec la mauvaise trame.
    //
    // Une fonction qui ment sur ce qu'elle renvoie est un piege permanent :
    // chaque moitie a l'air juste et la relecture ne voit rien. Dire « voici du
    // 44,1 stereo » laisse au moins l'appelant verifier et convertir.
    let out_rate = source_rate;
    let out_channels = source_channels;
    let total_frames = all_samples.len() as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = file_path,
        samples = all_samples.len(),
        rate = source_rate,
        channels = source_channels,
        duration_s,
        "decoded_symphonia"
    );

    Ok(DecodedAudio {
        samples_i32: all_samples,
        bit_depth: source_bd,
        sample_rate: out_rate,
        channels: out_channels,
        duration_s,
    })
}

/// Streaming DSD decode: reads the file in chunks, converts DSD→PCM
/// progressively, and sends PCM chunks through the channel.
///
/// Memory usage: O(block_size + filter_len) ≈ 40 KB regardless of file size.
/// This replaces the old batch path that loaded the entire DSD file (~200MB+)
/// into memory and then expanded it to f64 arrays (~13GB for a 5-min DSD64),
/// causing OOM crashes on Windows.
#[allow(clippy::too_many_arguments)]
fn decode_dsd_streaming(
    file_path: &str,
    ext: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    output_bd: u16,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    first_chunk_sent: &mut bool,
    data_ready: &Option<std::sync::Arc<tokio::sync::Notify>>,
    levels_tx: &Option<tokio::sync::mpsc::UnboundedSender<super::tap::RawWindow>>,
    rt: &tokio::runtime::Handle,
    seek_s: f64,
) -> Result<(u16, u32), String> {
    use super::dsd_to_pcm::DsdToPcmStreamer;

    // Parse header once, then create streamer + reader from the same info.
    let (dsd_rate, channels) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        (info.sample_rate, info.channels as usize)
    } else {
        let info = super::dff::parse_dff(file_path)?;
        (info.sample_rate, info.channels as usize)
    };
    let lsb_first = ext == "dsf";

    let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));
    let mut streamer = DsdToPcmStreamer::new(dsd_rate, output_rate, channels, lsb_first);
    let source_ch = checked_channels(channels as u32, "DSD stream source")?;
    let output_ch = checked_channels(
        target_channels.unwrap_or(channels as u32),
        "DSD stream target",
    )?;
    let mut pcm_adapter = StreamingPcmAdapter::new(
        24,
        source_ch as u32,
        output_ch as u32,
        output_rate,
        output_rate,
    )?;

    // Accumulate PCM output and flush in chunk_size batches
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(chunk_size * 2);
    let flush_len = frame_aligned_chunk_len(chunk_size, output_bd, output_ch);
    let mut total_output_samples = 0usize;

    // Inner loop: feed DSD chunks, convert PCM, send downstream.
    // Factored into a closure to avoid duplicating the flush logic.
    let mut process_dsd_chunk =
        |streamer: &mut DsdToPcmStreamer, dsd_chunk: &[u8]| -> Result<bool, String> {
            let pcm_24 = streamer.feed(dsd_chunk);
            if pcm_24.is_empty() {
                return Ok(false);
            }
            let samples: Vec<i32> = pcm_24
                .chunks_exact(3)
                .map(|c| {
                    let value = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16);
                    (value << 8) >> 8
                })
                .collect();
            let adapted = pcm_adapter.push(&samples)?;
            total_output_samples += adapted.len();
            append_pcm_samples(&mut pcm_buf, &adapted, 24, output_bd);
            while pcm_buf.len() >= flush_len {
                let chunk: Vec<u8> = pcm_buf.drain(..flush_len).collect();
                // Send PCM data first, compute levels after (same rationale
                // as the symphonia path: avoid delaying the audio stream).
                match rt.block_on(tokio::time::timeout(
                    std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                    tx.send(chunk.clone()),
                )) {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        debug!("dsd_streaming_consumer_dropped");
                        return Ok(true); // consumer gone
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_secs = SEND_TIMEOUT_SECS,
                            "dsd_streaming_send_timeout"
                        );
                        return Ok(true); // channel stalled
                    }
                }
                if !*first_chunk_sent {
                    *first_chunk_sent = true;
                    if let Some(n) = data_ready {
                        n.notify_one();
                    }
                }
                if let Some(ltx) = levels_tx {
                    super::tap::send_windowed_pcm(ltx, &chunk, output_bd, output_ch, output_rate);
                }
            }
            Ok(false)
        };

    // Read and process DSD data in chunks
    if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let mut reader = super::dsf::DsfStreamReader::open(file_path, info)?;
        if seek_s > 0.0 {
            // Block-aligned seek so DSD playback resumes at the requested
            // position instead of restarting at 0 (Xavier). bytes-per-channel =
            // seek_s × dsd_rate / 8.
            let target_bpc = (seek_s * dsd_rate as f64 / 8.0) as usize;
            let reached = reader.seek_to_bytes_per_channel(target_bpc)?;
            tracing::info!(
                seek_s,
                dsd_rate,
                target_bpc,
                reached_bpc = reached,
                "dsd_streaming_seek_block_aligned"
            );
        }
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_dsd_chunk(&mut streamer, &dsd_chunk)? {
                return Ok((output_bd, output_rate));
            }
        }
    } else {
        let info = super::dff::parse_dff(file_path)?;
        // Read in chunks aligned to channel count.
        // 32768 bytes is a good balance: small enough for low memory, large
        // enough to amortize I/O overhead.
        let read_chunk = 32768 / channels * channels;
        let mut reader = super::dff::DffStreamReader::open(file_path, &info, read_chunk)?;
        if seek_s > 0.0 {
            let target = (seek_s * dsd_rate as f64 / 8.0) as usize * channels;
            let reached = reader.seek_to_interleaved_byte(target, channels)?;
            tracing::info!(
                seek_s,
                dsd_rate,
                target,
                reached,
                "dsd_streaming_seek_block_aligned"
            );
        }
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_dsd_chunk(&mut streamer, &dsd_chunk)? {
                return Ok((output_bd, output_rate));
            }
        }
    }

    drop(process_dsd_chunk);

    // Flush remaining samples from the DSD FIR filter through the same channel
    // adapter as the body.
    let tail = streamer.flush();
    if !tail.is_empty() {
        let samples: Vec<i32> = tail
            .chunks_exact(3)
            .map(|c| {
                let value = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16);
                (value << 8) >> 8
            })
            .collect();
        let adapted = pcm_adapter.push(&samples)?;
        total_output_samples += adapted.len();
        append_pcm_samples(&mut pcm_buf, &adapted, 24, output_bd);
    }
    let adapter_tail = pcm_adapter.finish()?;
    total_output_samples += adapter_tail.len();
    append_pcm_samples(&mut pcm_buf, &adapter_tail, 24, output_bd);

    // Send any remaining bytes (send first, levels after)
    if !pcm_buf.is_empty() {
        let send_ok = match rt.block_on(tokio::time::timeout(
            std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
            tx.send(pcm_buf.clone()),
        )) {
            Ok(Ok(())) => true,
            Ok(Err(_)) => {
                debug!("dsd_streaming_consumer_dropped (final)");
                false
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = SEND_TIMEOUT_SECS,
                    "dsd_streaming_send_timeout (final)"
                );
                false
            }
        };
        if send_ok {
            if let Some(ltx) = levels_tx {
                super::tap::send_windowed_pcm(ltx, &pcm_buf, output_bd, output_ch, output_rate);
            }
        }
    }

    let total_samples = total_output_samples;
    let total_frames = total_samples as f64 / output_ch as f64;
    let duration_s = total_frames / output_rate as f64;

    debug!(
        file = file_path,
        ext,
        dsd_rate,
        output_rate,
        source_channels = channels,
        output_channels = output_ch,
        total_samples,
        duration_s,
        "decoded_dsd_streaming"
    );

    Ok((output_bd, output_rate))
}

/// Streaming DSD-to-DoP encoder. Reads DSD from DSF/DFF files and outputs
/// 24-bit PCM with DoP markers, ready for WASAPI/ASIO/CoreAudio at 176.4/352.8kHz.
pub fn decode_dsd_to_dop_streaming(
    file_path: &str,
    ext: &str,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    first_chunk_sent: &mut bool,
    data_ready: &Option<std::sync::Arc<tokio::sync::Notify>>,
    rt: &tokio::runtime::Handle,
) -> Result<(u16, u32), String> {
    use super::dsd_to_dop::DsdToDoP;

    let (dsd_rate, channels) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        (info.sample_rate, info.channels as usize)
    } else {
        let info = super::dff::parse_dff(file_path)?;
        (info.sample_rate, info.channels as usize)
    };
    let lsb_first = ext == "dsf";
    let dop_rate = DsdToDoP::dop_rate(dsd_rate);
    let mut encoder = DsdToDoP::new(channels, lsb_first);
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(chunk_size * 2);
    let flush_len = frame_aligned_chunk_len(chunk_size, 24, channels as u16);

    let mut process_chunk = |dsd_chunk: &[u8]| -> Result<bool, String> {
        let dop_bytes = encoder.feed(dsd_chunk);
        if dop_bytes.is_empty() {
            return Ok(false);
        }
        pcm_buf.extend_from_slice(&dop_bytes);
        while pcm_buf.len() >= flush_len {
            let chunk: Vec<u8> = pcm_buf.drain(..flush_len).collect();
            match rt.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                tx.send(chunk),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Ok(true),
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = SEND_TIMEOUT_SECS,
                        "dop_streaming_send_timeout"
                    );
                    return Ok(true);
                }
            }
            if !*first_chunk_sent {
                *first_chunk_sent = true;
                if let Some(n) = data_ready {
                    n.notify_one();
                }
            }
        }
        Ok(false)
    };

    if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let mut reader = super::dsf::DsfStreamReader::open(file_path, info)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_chunk(&dsd_chunk)? {
                return Ok((24, dop_rate));
            }
        }
    } else {
        let info = super::dff::parse_dff(file_path)?;
        let read_chunk = 32768 / channels * channels;
        let mut reader = super::dff::DffStreamReader::open(file_path, &info, read_chunk)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_chunk(&dsd_chunk)? {
                return Ok((24, dop_rate));
            }
        }
    }

    if !pcm_buf.is_empty() {
        let _ = rt.block_on(tokio::time::timeout(
            std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
            tx.send(pcm_buf),
        ));
    }

    debug!(
        file = file_path,
        dsd_rate, dop_rate, channels, "decoded_dsd_dop_streaming"
    );
    Ok((24, dop_rate))
}

/// Decode a DSD file (DSF or DFF) to PCM using streaming converter.
///
/// Uses `DsdToPcmStreamer` to process the file in chunks, avoiding the
/// catastrophic memory usage of the old batch approach.
/// Memory usage: O(block_size + filter_len) ≈ 40 KB regardless of file size.
/// Upper bound of output PCM samples needed for a `seek_s`..`seek_s+max_duration_s`
/// window, so a bounded read (e.g. a 10 s preview or the embedding window) stops
/// decoding early instead of converting the WHOLE DSD file. The DSD→PCM path runs
/// a per-output-sample FIR: a full DSD64 track is billions of multiply-adds on a
/// non-cancellable thread, which reads as a hang. Returns `usize::MAX` when no
/// duration is requested, so the "decode everything" path is unchanged.
fn dsd_needed_samples(
    seek_s: f64,
    max_duration_s: f64,
    output_rate: u32,
    channels: usize,
) -> usize {
    if max_duration_s <= 0.0 {
        return usize::MAX;
    }
    let skip_frames = if seek_s > 0.0 {
        (seek_s * output_rate as f64) as usize
    } else {
        0
    };
    let keep_frames = (max_duration_s * output_rate as f64).ceil() as usize;
    skip_frames
        .saturating_add(keep_frames)
        .saturating_mul(channels.max(1))
}

fn decode_dsd_to_pcm(
    file_path: &str,
    ext: &str,
    target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    use super::dsd_to_pcm::DsdToPcmStreamer;

    // Convert the streamer's 24-bit LE output bytes straight to i32 samples,
    // chunk by chunk, so we never accumulate a second full-file byte buffer
    // (a DSD64 album track is hundreds of MB; the old code held both the bytes
    // and the i32 samples, then copied a third time via `to_vec`).
    let append_pcm24 = |dst: &mut Vec<i32>, bytes: &[u8]| {
        for c in bytes.chunks_exact(3) {
            let v = c[0] as u32 | ((c[1] as u32) << 8) | ((c[2] as u32) << 16);
            dst.push(if v & 0x80_0000 != 0 {
                (v | 0xFF00_0000) as i32
            } else {
                v as i32
            });
        }
    };

    let mut all_samples: Vec<i32> = Vec::new();

    let (dsd_rate, output_rate, channels) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let dsd_rate = info.sample_rate;
        let channels = info.channels as usize;
        let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));
        // Pre-reserve the whole output so the Vec doesn't repeatedly reallocate
        // as it grows: output samples ≈ (dsd_samples / decimation) * channels.
        let decimation = (dsd_rate / output_rate).max(1) as u64;
        all_samples.reserve((info.total_samples / decimation) as usize * channels);
        let mut streamer = DsdToPcmStreamer::new(dsd_rate, output_rate, channels, true);
        let mut reader = super::dsf::DsfStreamReader::open(file_path, info)?;
        let needed = dsd_needed_samples(seek_s, max_duration_s, output_rate, channels);
        while let Some(dsd_chunk) = reader.next_chunk()? {
            append_pcm24(&mut all_samples, &streamer.feed(&dsd_chunk));
            if all_samples.len() >= needed {
                break;
            }
        }
        // Skip the filter flush once the window is satisfied (its tail would be
        // trimmed away anyway); only a full-file read needs it.
        if all_samples.len() < needed {
            append_pcm24(&mut all_samples, &streamer.flush());
        }
        (dsd_rate, output_rate, channels)
    } else {
        let info = super::dff::parse_dff(file_path)?;
        let dsd_rate = info.sample_rate;
        let channels = info.channels as usize;
        let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));
        // DFF has no explicit sample count; estimate from the data chunk size
        // (data_size bytes * 8 DSD samples/byte, then decimated).
        let decimation = (dsd_rate / output_rate).max(1) as u64;
        all_samples.reserve((info.data_size.saturating_mul(8) / decimation) as usize);
        let mut streamer = DsdToPcmStreamer::new(dsd_rate, output_rate, channels, false);
        let read_chunk = 32768 / channels * channels;
        let mut reader = super::dff::DffStreamReader::open(file_path, &info, read_chunk)?;
        let needed = dsd_needed_samples(seek_s, max_duration_s, output_rate, channels);
        while let Some(dsd_chunk) = reader.next_chunk()? {
            append_pcm24(&mut all_samples, &streamer.feed(&dsd_chunk));
            if all_samples.len() >= needed {
                break;
            }
        }
        if all_samples.len() < needed {
            append_pcm24(&mut all_samples, &streamer.flush());
        }
        (dsd_rate, output_rate, channels)
    };

    // Apply seek and duration limits on the output PCM
    let skip_frames = if seek_s > 0.0 {
        (seek_s * output_rate as f64) as usize
    } else {
        0
    };
    let skip_samples = skip_frames * channels;

    let max_frames = if max_duration_s > 0.0 {
        (max_duration_s * output_rate as f64) as usize
    } else {
        usize::MAX
    };
    let max_samples = max_frames.saturating_mul(channels);

    let start = skip_samples.min(all_samples.len());
    let end = start.saturating_add(max_samples).min(all_samples.len());
    let out_len = end - start;

    let actual_frames = out_len / channels;
    let actual_duration = actual_frames as f64 / output_rate as f64;

    debug!(
        file = file_path,
        ext,
        dsd_rate,
        output_rate,
        channels,
        samples = out_len,
        duration_s = actual_duration,
        "decoded_dsd_native"
    );

    // Trim in place (drop tail then head) so the fully-decoded buffer is
    // returned without allocating a second copy. With no seek (start == 0) the
    // drain is a no-op and the Vec is moved out directly.
    all_samples.truncate(end);
    all_samples.drain(..start);

    Ok(DecodedAudio {
        samples_i32: all_samples,
        bit_depth: 24,
        sample_rate: output_rate,
        channels: channels as u32,
        duration_s: actual_duration,
    })
}

#[cfg(test)]
mod decode_integration_tests {

    // --- StageCache : budget + éviction LRU (chantier lenteurs Yves) -------

    fn faux_stage(bytes: u64) -> std::sync::Arc<super::StagedFile> {
        // Chemin bidon : StagedFile::drop appellera remove_file dessus, qui
        // échoue sans conséquence (le fichier n'existe pas).
        std::sync::Arc::new(super::StagedFile {
            path: std::path::PathBuf::from(format!("/tmp/tune-test-inexistant-{bytes}")),
            bytes,
        })
    }

    fn cle(n: &str) -> super::StageKey {
        super::StageKey {
            src: n.to_string(),
            mtime: 0,
            size: 0,
        }
    }

    #[test]
    fn le_cache_evince_le_plus_ancien_quand_le_budget_est_depasse() {
        let mut c = super::StageCache::new(100);
        c.insert(cle("a"), faux_stage(60));
        c.insert(cle("b"), faux_stage(30)); // total 90, ok
        assert_eq!(c.map.len(), 2);
        c.insert(cle("c"), faux_stage(30)); // total 120 > 100 → évince "a"
        assert!(!c.map.contains_key(&cle("a")), "le plus ancien doit partir");
        assert!(c.map.contains_key(&cle("b")) && c.map.contains_key(&cle("c")));
        assert_eq!(c.bytes, 60, "octets recomptés après éviction");
    }

    #[test]
    fn touch_protege_l_entree_recemment_utilisee_de_l_eviction() {
        let mut c = super::StageCache::new(100);
        c.insert(cle("a"), faux_stage(50));
        c.insert(cle("b"), faux_stage(50));
        // "a" redevient la plus récente : c'est "b" qui doit tomber.
        c.touch(&cle("a"));
        c.insert(cle("c"), faux_stage(50)); // 150 > 100 → évince le plus ancien = "b"
        assert!(c.map.contains_key(&cle("a")), "a, réutilisée, survit");
        assert!(!c.map.contains_key(&cle("b")), "b, la plus ancienne, part");
    }

    #[test]
    fn le_cache_garde_toujours_au_moins_la_derniere_entree() {
        // Un fichier plus gros que le budget entier ne doit pas s'auto-évincer
        // (sinon on l'aurait copié pour rien) : la boucle s'arrête à 1 entrée.
        let mut c = super::StageCache::new(10);
        c.insert(cle("enorme"), faux_stage(500));
        assert_eq!(c.map.len(), 1, "la dernière entrée reste, budget ou pas");
    }

    // --- montage_reseau_depuis_mounts (chantier lenteurs, 24/08) -----------

    const MOUNTS_18: &str = "\
/dev/mapper/ubuntu--vg-ubuntu--lv / ext4 rw,relatime 0 0
/dev/sda2 /boot ext4 rw,relatime 0 0
/dev/sdb1 /data ext4 rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid 0 0
//192.168.1.55/share /mnt/eversolo_nvme cifs rw,vers=3.0 0 0
nas:/volume1/music /mnt/nas nfs4 rw,relatime 0 0
";

    #[test]
    fn un_disque_local_dedie_nest_pas_un_montage_reseau() {
        // LE cas de la .18 : /data/music est un AUTRE périphérique que /tmp
        // (sdb1 contre le LV système), et l'ancien critère `st_dev` copiait
        // chaque .dsf de 300 Mo avant la moindre note. ext4 local = on lit
        // sur place.
        assert!(!montage_reseau_depuis_mounts(
            MOUNTS_18,
            "/data/music/V_DSF/Classique/101 - Lachrimae Antiquae.dsf"
        ));
    }

    #[test]
    fn cifs_et_nfs_sont_des_montages_reseau() {
        // Le NVMe de Jérôme derrière l'Eversolo, monté en CIFS.
        assert!(montage_reseau_depuis_mounts(
            MOUNTS_18,
            "/mnt/eversolo_nvme/77A6-799D/album/titre.flac"
        ));
        // Le NAS d'Yves en NFS — le cas qui a motivé le staging (90 s+ par
        // piste en WiFi sans lui).
        assert!(montage_reseau_depuis_mounts(
            MOUNTS_18,
            "/mnt/nas/musique/a.flac"
        ));
    }

    #[test]
    fn le_point_de_montage_le_plus_long_gagne() {
        // /mnt/eversolo_nvme (cifs) doit l'emporter sur / (ext4) — et un
        // chemin qui n'a que / comme préfixe reste local.
        assert!(!montage_reseau_depuis_mounts(
            MOUNTS_18,
            "/home/jerome/musique/a.flac"
        ));
        // Préfixe TEXTUEL sans être un composant : /data-nas n'est pas /data.
        let mounts = "nas:/v /data nfs4 rw 0 0\n/dev/sda1 / ext4 rw 0 0\n";
        assert!(!montage_reseau_depuis_mounts(mounts, "/data-locale/a.flac"));
        assert!(montage_reseau_depuis_mounts(mounts, "/data/a.flac"));
    }

    #[test]
    fn dans_le_doute_on_ne_copie_pas() {
        // Contenu illisible, vide, ou chemin hors de tout montage connu :
        // répondre « local » — ne pas copier est le choix le moins coûteux.
        assert!(!montage_reseau_depuis_mounts("", "/data/music/a.flac"));
        assert!(!montage_reseau_depuis_mounts("garbage\n", "/x/a.flac"));
        // fuseblk = NTFS local via FUSE : PAS un montage réseau.
        let mounts = "/dev/sdc1 /mnt/usb fuseblk rw 0 0\n";
        assert!(!montage_reseau_depuis_mounts(mounts, "/mnt/usb/a.flac"));
    }

    use super::*;
    use std::path::PathBuf;

    #[test]
    fn frame_aligned_chunk_len_fixes_24bit_stereo_32768() {
        // The historical drain size that pegged OAAT VU meters.
        assert_eq!(32768 % 6, 2);
        assert_eq!(frame_aligned_chunk_len(32768, 24, 2), 32766);
        assert_eq!(frame_aligned_chunk_len(32768, 16, 2), 32768);
        assert_eq!(frame_aligned_chunk_len(32768, 32, 2), 32768);
        assert_eq!(frame_aligned_chunk_len(32766, 24, 2) % 6, 0);
    }

    fn fixture_path(name: &str) -> String {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures");
        p.push(name);
        p.to_string_lossy().to_string()
    }

    /// Write a minimal valid stereo DSD64 .dsf file to `path`, filling every
    /// DSD byte with `pattern`. One super-block per channel (4096 bytes each).
    fn write_test_dsf(path: &str, pattern: u8) {
        let channels: u32 = 2;
        let sample_rate: u32 = 2_822_400;
        let block_size: u32 = 4096;
        let total_samples: u64 = block_size as u64 * 8; // DSD samples per channel
        // Data: ch0 block then ch1 block (block-interleaved).
        let mut data = Vec::new();
        for _ in 0..channels {
            data.extend(std::iter::repeat_n(pattern, block_size as usize));
        }

        let mut buf = Vec::new();
        // DSD chunk (28 bytes)
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes());
        buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // no metadata
        // fmt chunk (52 bytes)
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u32.to_le_bytes()); // format id = DSD raw
        buf.extend_from_slice(&2u32.to_le_bytes()); // channel type = stereo
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        buf.extend_from_slice(&total_samples.to_le_bytes());
        buf.extend_from_slice(&block_size.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        // data chunk
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&data);

        std::fs::write(path, &buf).unwrap();
    }

    // End-to-end DSD decode: exercises parse_dsf + DsfStreamReader block
    // deinterleave + DsdToPcmStreamer FIR (incl. the fast path) + the
    // chunk->i32 conversion and in-place trim. No real .dsf fixtures existed.
    #[test]
    fn decode_dsf_end_to_end_silence() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let path = dsf.path().to_path_buf();
        let p = path.to_str().unwrap();
        // 0x55 = 01010101, LSB-first -> alternating +1/-1 -> near silence.
        write_test_dsf(p, 0x55);
        let out = decode_dsd_to_pcm(p, "dsf", Some(176_400), None, 0.0, 0.0).unwrap();

        assert_eq!(out.sample_rate, 176_400);
        assert_eq!(out.bit_depth, 24);
        assert_eq!(out.channels, 2);
        assert!(!out.samples_i32.is_empty(), "should produce PCM");

        let n = out.samples_i32.len();
        let mut max_abs = 0i32;
        for &s in &out.samples_i32[n / 4..3 * n / 4] {
            max_abs = max_abs.max(s.abs());
        }
        // 24-bit full scale is 8_388_607; alternating DSD must be near silence.
        assert!(
            max_abs < 300_000,
            "alternating DSD should be near silence, got {max_abs}"
        );
    }

    #[test]
    fn decode_dsf_end_to_end_negative_dc() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let path = dsf.path().to_path_buf();
        let p = path.to_str().unwrap();
        // 0x00 = all bits 0 -> all -1.0 -> strong negative DC.
        write_test_dsf(p, 0x00);
        let out = decode_dsd_to_pcm(p, "dsf", Some(176_400), None, 0.0, 0.0).unwrap();

        assert!(!out.samples_i32.is_empty());
        let mid = out.samples_i32[out.samples_i32.len() / 2];
        assert!(
            mid < -3_000_000,
            "all-zero DSD should decode to strong negative PCM, got {mid}"
        );
    }

    #[test]
    fn decode_dsf_stereo_vers_mono_respecte_les_trames() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let path = dsf.path().to_path_buf();
        let p = path.to_str().unwrap();
        write_test_dsf(p, 0x55);
        let stereo = decode_to_pcm(p, Some(176_400), None, 0.0, 0.0).unwrap();
        let mono = decode_to_pcm(p, Some(176_400), Some(1), 0.0, 0.0).unwrap();

        assert_eq!(stereo.channels, 2);
        assert_eq!(mono.channels, 1);
        assert_eq!(mono.sample_rate, 176_400);
        assert_eq!(
            mono.samples_i32.len(),
            stereo.samples_i32.len() / 2,
            "le downmix change l'entrelacement, pas le nombre de trames"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_dsf_mono_entete_et_payload_sont_alignes() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let path = dsf.path().to_path_buf();
        write_test_dsf(path.to_str().unwrap(), 0x55);
        let path_for_decode = path.to_string_lossy().to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let decoder = tokio::task::spawn_blocking(move || {
            decode_to_pcm_streaming_inner(
                &path_for_decode,
                Some(176_400),
                Some(1),
                Some(24),
                tx,
                1_024,
                None,
                None,
                0.0,
                None,
            )
        });

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        assert_eq!(decoder.await.unwrap().unwrap(), (24, 176_400));

        let header = chunks.first().expect("WAV header DSD->PCM");
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            176_400
        );
        assert_eq!(u16::from_le_bytes([header[34], header[35]]), 24);
        assert!(chunks.len() > 1);
        assert!(chunks[1..].iter().all(|chunk| chunk.len() % 3 == 0));
    }

    #[test]
    fn dsd_needed_samples_bounds() {
        // No duration requested → decode the whole file (unchanged path).
        assert_eq!(dsd_needed_samples(0.0, 0.0, 48_000, 2), usize::MAX);
        // 10 s @ 48 kHz stereo = 480_000 frames × 2 channels.
        assert_eq!(dsd_needed_samples(0.0, 10.0, 48_000, 2), 480_000 * 2);
        // Seek adds its frames to the window; mono keeps the frame count.
        assert_eq!(dsd_needed_samples(1.0, 10.0, 48_000, 1), 48_000 + 480_000);
    }

    // A bounded read must stop decoding early — decode less than the full file
    // instead of converting all of it and trimming (the DSD hang, Sandro's DSF).
    #[test]
    fn decode_dsf_bounded_stops_early() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let path = dsf.path().to_path_buf();
        let p = path.to_str().unwrap();
        write_test_dsf(p, 0x55);
        let full = decode_dsd_to_pcm(p, "dsf", Some(176_400), None, 0.0, 0.0).unwrap();
        let bounded = decode_dsd_to_pcm(p, "dsf", Some(176_400), None, 0.0, 0.005).unwrap();

        assert!(!bounded.samples_i32.is_empty());
        assert!(
            bounded.samples_i32.len() < full.samples_i32.len(),
            "bounded decode must be shorter than full: {} vs {}",
            bounded.samples_i32.len(),
            full.samples_i32.len(),
        );
        let frames = bounded.samples_i32.len() / bounded.channels.max(1) as usize;
        assert!(
            frames as f64 / 176_400.0 <= 0.006,
            "bounded window should be ~0.005 s, got {} frames",
            frames,
        );
    }

    #[test]
    fn can_decode_native_formats() {
        assert!(can_decode_native("song.flac"));
        assert!(can_decode_native("song.mp3"));
        assert!(can_decode_native("song.wav"));
        assert!(can_decode_native("song.m4a"));
        assert!(can_decode_native("song.ogg"));
        assert!(can_decode_native("song.oga"));
        assert!(can_decode_native("song.opus"));
        assert!(can_decode_native("song.aiff"));
        assert!(can_decode_native("song.aif"));
        assert!(can_decode_native("song.dsf"));
        assert!(can_decode_native("song.dff"));
        assert!(can_decode_native("song.ape"));
        assert!(can_decode_native("song.wv"));
        assert!(!can_decode_native("song.wma")); // aucun décodeur WMA livré
    }

    #[test]
    fn dff_dst_n_est_jamais_annonce_ni_envoye_comme_decodable() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tune_decode_dff_dst_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("album.dff");
        std::fs::write(&path, super::super::support::dff_dst_minimal_fixture()).unwrap();

        assert!(!can_decode_native(path.to_str().unwrap()));
        let error = match decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0) {
            Ok(_) => panic!("un DFF/DST ne doit jamais atteindre le décodeur DSD brut"),
            Err(error) => error,
        };
        assert!(error.contains("aucun décodeur DST"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_wav() {
        let path = fixture_path("test.wav");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(!result.samples_i32.is_empty(), "WAV should produce samples");
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 0.9 && result.duration_s < 1.1,
            "duration should be ~1s, got {}",
            result.duration_s
        );
    }

    /// #1498 reste vrai sans cible : les métadonnées décrivent la source.
    #[test]
    fn decode_sans_cible_decrit_la_source() {
        let path = fixture_path("test.wav");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert_eq!(result.sample_rate, 44_100);
        assert_eq!(result.channels, 2);
        let frames = result.samples_i32.len() as f64 / result.channels as f64;
        let duration = frames / result.sample_rate as f64;
        assert!(
            (duration - result.duration_s).abs() < 0.01,
            "duree deduite de l'etiquette ({duration:.3} s) et duree rapportee ({:.3} s) doivent concorder",
            result.duration_s
        );
    }

    /// #2230 — contre-épreuve principale : une vraie source PCM 96 kHz, 24 bits,
    /// stéréo devient 44,1 kHz mono. Cadence, trames, entrelacement et taille du
    /// payload doivent raconter la même chose.
    #[test]
    fn la_cible_est_un_contrat_sur_le_payload_symphonia() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("stereo_96000_24.wav");
        ecrire_wav_96k_24_stereo(&path, 9_600);

        let d = decode_to_pcm(path.to_str().unwrap(), Some(44_100), Some(1), 0.0, 0.0).unwrap();

        assert_eq!(d.sample_rate, 44_100);
        assert_eq!(d.channels, 1);
        assert_eq!(d.bit_depth, 24);
        assert_eq!(d.samples_i32.len(), 4_410, "0,1 s doit rester 0,1 s");
        assert_eq!(d.pcm_bytes().len(), 4_410 * 3);
        assert!((d.duration_s - 0.1).abs() < 0.000_001);
        assert!(d.samples_i32.iter().any(|&sample| sample != 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_wav_entete_et_payload_partagent_le_format_cible() {
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let path = wav.path().to_path_buf();
        ecrire_wav_96k_24_stereo(&path, 9_600);
        let path_for_decode = path.to_string_lossy().to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let decoder = tokio::task::spawn_blocking(move || {
            decode_to_pcm_streaming_inner(
                &path_for_decode,
                Some(44_100),
                Some(1),
                Some(16),
                tx,
                1_024,
                None,
                None,
                0.0,
                None,
            )
        });

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        let result = decoder.await.unwrap().unwrap();

        assert_eq!(result, (16, 44_100));
        let header = chunks.first().expect("WAV header");
        assert_eq!(header.len(), 44);
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            44_100
        );
        assert_eq!(u16::from_le_bytes([header[32], header[33]]), 2);
        assert_eq!(u16::from_le_bytes([header[34], header[35]]), 16);

        let payload_chunks = &chunks[1..];
        assert!(!payload_chunks.is_empty());
        assert!(payload_chunks.iter().all(|chunk| chunk.len() % 2 == 0));
        let payload_len = payload_chunks.iter().map(Vec::len).sum::<usize>();
        let frames = payload_len / 2;
        assert!(
            (3_500..=5_500).contains(&frames),
            "0,1 s rééchantillonnée avec la latence sinc doit rester bornée, obtenu {frames} trames"
        );
    }

    fn ecrire_wav_96k_24_stereo(path: &std::path::Path, frames: usize) {
        let mut donnees = Vec::with_capacity(frames * 6);
        for frame in 0..frames {
            let left = ((frame as i32 % 200) - 100) * 40_000;
            let right = left / 2;
            for sample in [left, right] {
                let bytes = sample.to_le_bytes();
                donnees.extend_from_slice(&bytes[..3]);
            }
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&2u16.to_le_bytes()); // stereo
        w.extend_from_slice(&96_000u32.to_le_bytes());
        w.extend_from_slice(&(96_000u32 * 6).to_le_bytes());
        w.extend_from_slice(&6u16.to_le_bytes()); // block align
        w.extend_from_slice(&24u16.to_le_bytes()); // bits
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(&donnees);
        std::fs::write(path, w).unwrap();
    }

    #[test]
    fn decode_flac() {
        let path = fixture_path("test.flac");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(
            !result.samples_i32.is_empty(),
            "FLAC should produce samples"
        );
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert!(result.duration_s > 0.9, "duration should be ~1s");
    }

    #[test]
    fn decode_mp3() {
        let path = fixture_path("test.mp3");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(!result.samples_i32.is_empty(), "MP3 should produce samples");
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
    }

    #[test]
    fn decode_ogg() {
        // test.ogg is Ogg-FLAC — decoded natively by symphonia, NOT routed to
        // the Opus path. Assert a real ~2 s duration so a silent/empty decode
        // (the failure mode we guard against) fails the test loudly.
        let path = fixture_path("test.ogg");
        assert!(
            !ogg_stream_is_opus(&path),
            "test.ogg must not be sniffed as Opus"
        );
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(!result.samples_i32.is_empty(), "OGG should produce samples");
        assert_eq!(result.sample_rate, 44100);
        assert!(
            result.duration_s > 0.5,
            "ogg duration should be non-trivial, got {}",
            result.duration_s
        );
    }

    #[test]
    fn decode_ogg_vorbis() {
        // A genuine Ogg-Vorbis stream: must go through symphonia (Vorbis), not
        // the Opus decoder, and yield ~2 s of real audio.
        let path = fixture_path("test_vorbis.ogg");
        assert!(
            !ogg_stream_is_opus(&path),
            "Ogg-Vorbis must not be sniffed as Opus"
        );
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(
            !result.samples_i32.is_empty(),
            "Ogg-Vorbis should produce samples"
        );
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 1.8 && result.duration_s < 2.2,
            "Ogg-Vorbis duration should be ~2 s, got {}",
            result.duration_s
        );
        assert!(
            result.samples_i32.iter().any(|s| *s != 0),
            "Ogg-Vorbis PCM must not be all silence"
        );
    }

    #[test]
    fn decode_chained_ogg_vorbis_decodes_past_first_link() {
        // Two Ogg-Vorbis physical streams back-to-back (an icecast rip, or
        // `cat a.ogg b.ogg`). Symphonia signals the mid-file boundary with
        // `Error::ResetRequired`; treating it as EOF truncated playback to the
        // first link, the local output signalled a natural end a few seconds
        // in and the poller replayed the head of the track over and over —
        // #1270 « boucle de 2-3 s en début de piste » (liste Bertrand 13/08).
        let single = std::fs::read(fixture_path("test_vorbis.ogg")).unwrap();
        let path =
            std::env::temp_dir().join(format!("tune_chained_vorbis_{}.ogg", std::process::id()));
        let mut chained = single.clone();
        chained.extend_from_slice(&single);
        std::fs::write(&path, &chained).unwrap();

        let result = decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
        let _ = std::fs::remove_file(&path);
        let result = result.unwrap();

        // Each link is ~2 s: the chained file must decode BOTH (~4 s), not
        // stop at the first boundary (~2 s).
        assert!(
            result.duration_s > 3.0,
            "chained Ogg-Vorbis must decode past the first chain boundary, got {} s",
            result.duration_s
        );
        assert!(
            result.samples_i32.iter().any(|s| *s != 0),
            "chained Ogg-Vorbis PCM must not be all silence"
        );
    }

    #[test]
    fn ogg_opus_is_sniffed_as_opus() {
        // A .ogg carrying Opus must be detected so it routes to libopus, not
        // the Vorbis decoder (which would fail → silence + 2 s loop).
        assert!(ogg_stream_is_opus(&fixture_path("test_opus_in.ogg")));
        assert!(ogg_stream_is_opus(&fixture_path("test.opus")));
    }

    #[test]
    fn decode_opus() {
        // Native .opus (Ogg-Opus): decoded by libopus at 48 kHz / 16-bit.
        let path = fixture_path("test.opus");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(
            !result.samples_i32.is_empty(),
            "Opus should produce samples"
        );
        assert_eq!(result.sample_rate, 48000, "Opus decodes at native 48 kHz");
        assert_eq!(result.bit_depth, 16);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 1.8 && result.duration_s < 2.2,
            "Opus duration should be ~2 s, got {}",
            result.duration_s
        );
        assert!(
            result.samples_i32.iter().any(|s| *s != 0),
            "decoded Opus PCM must not be all silence (the pre-fix failure mode)"
        );
    }

    #[test]
    fn decode_chained_ogg_opus_decodes_past_first_link() {
        // Two Ogg-Opus physical streams back-to-back (an icecast rip, or
        // `cat a.opus b.opus`). Symphonia signals the mid-file boundary with
        // `Error::ResetRequired`; the Opus loop treated it as EOF, truncating
        // playback to the first link — natural end a few seconds in, poller
        // replays the head of the track: the same « boucle de 2-3 s » of
        // #1270 that #1632 fixed for Vorbis, on the libopus path this time.
        let single = std::fs::read(fixture_path("test.opus")).unwrap();
        let path =
            std::env::temp_dir().join(format!("tune_chained_opus_{}.opus", std::process::id()));
        let mut chained = single.clone();
        chained.extend_from_slice(&single);
        std::fs::write(&path, &chained).unwrap();

        let result = decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0);
        let _ = std::fs::remove_file(&path);
        let result = result.unwrap();

        // Each link is ~2 s: the chained file must decode BOTH (~4 s), not
        // stop at the first boundary (~2 s).
        assert!(
            result.duration_s > 3.0,
            "chained Ogg-Opus must decode past the first chain boundary, got {} s",
            result.duration_s
        );
        assert!(
            result.samples_i32.iter().any(|s| *s != 0),
            "chained Ogg-Opus PCM must not be all silence"
        );
    }

    /// Empreinte du signal réellement décodé par libopus (#2251).
    ///
    /// `decode_opus` ci-dessus contrôle le contenant : cadence, canaux, durée,
    /// « pas tout à zéro ». Rien n'y contrôle le **contenu**. Or c'est
    /// exactement là que se cache le risque d'`audiopus_sys` : la crate est
    /// abandonnée (RUSTSEC-2026-0150) et devra être remplacée ; un remplaçant
    /// qui compile et rend le bon nombre d'échantillons du mauvais son
    /// passerait toute la suite actuelle sans un rouge.
    ///
    /// La fixture `test.opus` est un 440 Hz stéréo **asymétrique** : le canal
    /// gauche est deux fois plus fort que le droit. On vérifie donc la
    /// fréquence, le rapport gauche/droite, le niveau et le nombre de trames.
    /// On ne fige aucun condensat : le décodage Opus n'est pas garanti
    /// bit-à-bit d'une libopus à l'autre (RFC 8251), une empreinte spectrale
    /// tolérante l'est.
    ///
    /// ⚠ La libopus réellement liée **varie selon la machine** :
    /// `audiopus_sys` sonde `pkg-config` avant de retomber sur sa copie
    /// embarquée (figée sur xiph `7b05f44`, mars 2021). Un Mac de
    /// développement avec Homebrew lie la 1.6.1, un runner CI Linux la
    /// version du système ou la copie embarquée. Les bornes absolues
    /// ci-dessous sont donc larges (±20 %) : elles sont là pour attraper un
    /// gain divisé ou doublé, pas pour départager deux libopus conformes. Ce
    /// sont les grandeurs **relatives** — rapport gauche/droite, dominance
    /// spectrale, nombre de trames — qui portent la discrimination fine.
    ///
    /// Valeurs observées sur `audiopus 0.3.0-rc.0` / `audiopus_sys 0.2.2`
    /// (libopus 1.6.1 via Homebrew) : 96 960 trames, rms G = 2877,6,
    /// rms D = 1442,1, crête = 4140, dominante 440 Hz.
    #[test]
    fn decode_opus_fixture_garde_le_profil_du_signal() {
        use crate::audio::opus_ogg::{channel_f64, goertzel_power, rms};

        let path = fixture_path("test.opus");
        let d = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert_eq!(d.sample_rate, 48_000);
        assert_eq!(d.channels, 2);

        // Nombre de trames : contrat de découpage (pre-skip + granulepos).
        // Un remplaçant qui compte autrement doit être regardé de près, pas
        // adopté en silence.
        assert_eq!(
            d.samples_i32.len() / 2,
            96_960,
            "le nombre de trames décodées a changé — pre-skip ou fin de flux \
             traités différemment"
        );

        let left = channel_f64(&d.samples_i32, 2, 0);
        let right = channel_f64(&d.samples_i32, 2, 1);

        // Fenêtre de 0,5 s prise après l'attaque : 24 000 échantillons =
        // nombre entier de périodes à 440 et 880 Hz, Goertzel exact.
        let seg =
            |ch: &[f64]| -> Vec<f64> { ch.iter().skip(9_600).take(24_000).copied().collect() };
        for (name, ch) in [("gauche", &left), ("droit", &right)] {
            let s = seg(ch);
            assert_eq!(s.len(), 24_000, "fenêtre {name} incomplète");
            let p440 = goertzel_power(&s, 48_000.0, 440.0);
            let p880 = goertzel_power(&s, 48_000.0, 880.0);
            let p220 = goertzel_power(&s, 48_000.0, 220.0);
            assert!(
                p440 > 100.0 * p880.max(1.0) && p440 > 100.0 * p220.max(1.0),
                "canal {name} : la fixture est un 440 Hz, on mesure \
                 p220={p220:.1} p440={p440:.1} p880={p880:.1} — cadence fausse \
                 ou flux brouillé ?"
            );
        }

        // Asymétrie gauche/droite ≈ 2 : attrape l'inversion des canaux et le
        // repli mono, qu'aucun autre test de ce fichier ne verrait.
        let (rl, rr) = (rms(&left), rms(&right));
        let ratio = rl / rr.max(1.0);
        assert!(
            (1.8..=2.2).contains(&ratio),
            "rapport gauche/droite = {ratio:.2} (attendu ≈2,0 ; rms G={rl:.0}, \
             rms D={rr:.0}) — canaux inversés, repliés en mono, ou gain faux"
        );
        assert!(
            (2_300.0..=3_460.0).contains(&rl),
            "niveau du canal gauche dérivé : rms={rl:.0} (attendu ≈2878, ±20 %)"
        );

        let peak = d.samples_i32.iter().map(|s| s.abs()).max().unwrap_or(0);
        assert!(
            (3_310..=4_970).contains(&peak),
            "crête dérivée : {peak} (attendue ≈4140, ±20 %) — gain ou profondeur faux"
        );
    }

    #[test]
    fn decode_opus_stereo_vers_mono_garde_cadence_et_trames() {
        let path = fixture_path("test.opus");
        let stereo = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        let mono = decode_to_pcm(&path, Some(48_000), Some(1), 0.0, 0.0).unwrap();

        assert_eq!(mono.sample_rate, 48_000);
        assert_eq!(mono.channels, 1);
        assert_eq!(mono.bit_depth, 16);
        assert_eq!(mono.samples_i32.len(), stereo.samples_i32.len() / 2);
        assert_eq!(mono.pcm_bytes().len(), mono.samples_i32.len() * 2);
    }

    #[test]
    fn decode_opus_in_ogg_container() {
        // Opus muxed in a generic .ogg — the sniffer routes it to libopus.
        let path = fixture_path("test_opus_in.ogg");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert_eq!(result.sample_rate, 48000);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 1.8 && result.duration_s < 2.2,
            "Opus-in-ogg duration should be ~2 s, got {}",
            result.duration_s
        );
    }

    #[test]
    fn decode_opus_with_duration_limit() {
        let path = fixture_path("test.opus");
        let full = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        let bounded = decode_to_pcm(&path, None, None, 0.0, 0.5).unwrap();
        assert!(
            bounded.samples_i32.len() < full.samples_i32.len(),
            "bounded Opus decode must be shorter: {} vs {}",
            bounded.samples_i32.len(),
            full.samples_i32.len()
        );
        assert!(!bounded.samples_i32.is_empty());
        assert!(
            bounded.duration_s <= 0.55,
            "bounded window should be ~0.5 s, got {}",
            bounded.duration_s
        );
    }

    #[test]
    fn decode_opus_with_seek() {
        // Seek must drop the leading portion: a 0.5 s seek into a ~2 s file
        // yields ~1.5 s, and roughly `seek` seconds fewer frames than the full
        // decode (sample-accurate via packet pts).
        let path = fixture_path("test.opus");
        let full = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        let seeked = decode_to_pcm(&path, None, None, 0.5, 0.0).unwrap();
        assert!(
            seeked.samples_i32.len() < full.samples_i32.len(),
            "seeked Opus decode should have fewer samples"
        );
        let dropped_frames =
            (full.samples_i32.len() - seeked.samples_i32.len()) / full.channels.max(1) as usize;
        let dropped_s = dropped_frames as f64 / 48000.0;
        assert!(
            (dropped_s - 0.5).abs() < 0.1,
            "seek should drop ~0.5 s of audio, dropped {dropped_s} s"
        );
    }

    #[test]
    fn decode_m4a() {
        let path = fixture_path("test.m4a");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(!result.samples_i32.is_empty(), "M4A should produce samples");
        assert_eq!(result.sample_rate, 44100);
    }

    #[test]
    fn decode_aiff_native() {
        let path = fixture_path("test.aiff");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(
            !result.samples_i32.is_empty(),
            "AIFF should produce samples"
        );
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert!(
            result.duration_s > 0.9 && result.duration_s < 1.1,
            "duration should be ~1s, got {}",
            result.duration_s
        );
    }

    #[test]
    fn decode_with_duration_limit() {
        let path = fixture_path("test.wav");
        let full = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        let half = decode_to_pcm(&path, None, None, 0.0, 0.5).unwrap();
        assert!(
            half.samples_i32.len() < full.samples_i32.len(),
            "limited decode should have fewer samples"
        );
        assert!(
            half.samples_i32.len() > 0,
            "limited decode should still have samples"
        );
    }

    #[test]
    fn decode_with_seek() {
        let path = fixture_path("test.wav");
        let full = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        let seeked = decode_to_pcm(&path, None, None, 0.5, 0.0).unwrap();
        assert!(
            seeked.samples_i32.len() < full.samples_i32.len(),
            "seeked decode should have fewer samples"
        );
    }

    #[test]
    fn decode_nonexistent_file() {
        let result = decode_to_pcm("/nonexistent/file.flac", None, None, 0.0, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn cibles_pcm_invalides_sont_refusees_avant_lecture() {
        let path = "/nonexistent/validation_must_run_first.wav";
        assert!(
            decode_to_pcm(path, Some(0), Some(2), 0.0, 0.0)
                .err()
                .unwrap()
                .contains("sample rate")
        );
        assert!(
            decode_to_pcm(path, Some(44_100), Some(0), 0.0, 0.0)
                .err()
                .unwrap()
                .contains("channel count")
        );
    }

    #[test]
    fn dsf_is_native() {
        assert!(can_decode_native("test.dsf"));
        assert!(can_decode_native("test.dff"));
    }

    #[test]
    fn convert_pcm_bytes_16_to_32_widens_samples() {
        // Two 16-bit LE samples: 1 and -32768.
        let src = [0x01, 0x00, 0x00, 0x80];
        let out = convert_pcm_bytes(&src, 16, 32);
        // 1 << 16 = 0x0001_0000 ; -32768 << 16 = 0x8000_0000 (i32::MIN)
        assert_eq!(out, vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x80]);
    }

    #[test]
    fn convert_pcm_bytes_noop_when_same_depth() {
        let src = [0x12, 0x34, 0x56, 0x78];
        assert_eq!(convert_pcm_bytes(&src, 16, 16), src.to_vec());
    }

    #[test]
    fn convert_pcm_bytes_24_to_16_for_dlna_lpcm() {
        // #1137: a 24-bit source served to a DLNA renderer over the LPCM
        // fallback must be reduced to genuine 16-bit PCM, not relabelled.
        // 24-bit LE sample 0x123456 -> keep the top 16 bits -> 0x1234 (LE 34 12).
        // A negative sample 0x800000 (-8388608) -> 0x8000 (i16::MIN, LE 00 80).
        let src = [0x56, 0x34, 0x12, /* next */ 0x00, 0x00, 0x80];
        let out = convert_pcm_bytes(&src, 24, 16);
        assert_eq!(out, vec![0x34, 0x12, 0x00, 0x80]);
        // Output is exactly 2 bytes per sample (16-bit).
        assert_eq!(out.len(), 4);
    }

    // ---------------------------------------------------------------
    // #2157 — la profondeur source hors {16, 24, 32}
    // ---------------------------------------------------------------

    /// CONTRE-ÉPREUVE de non-régression : sur les trois profondeurs que la
    /// table énumérait, `requantize` doit rendre EXACTEMENT ce que rendaient
    /// les bras codés en dur. Si ce test tombe, le correctif a déplacé le
    /// niveau d'un flux qui marchait, et c'est le correctif qui est faux.
    #[test]
    fn requantize_reproduit_a_l_identique_les_trois_profondeurs_connues() {
        // Les couples (from, to) et le décalage que le code d'origine appliquait.
        let attendu: &[(u16, u16, i32)] = &[
            (24, 32, 8),   // `*s << 8`
            (16, 32, 16),  // `(*s as i32) << 16`
            (32, 32, 0),   // `*s`
            (32, 24, -8),  // `*s >> 8`
            (16, 24, 8),   // `(*s as i32) << 8`
            (24, 24, 0),   // `*s`
            (32, 16, -16), // `(*s >> 16) as i16`
            (24, 16, -8),  // `(*s >> 8) as i16`
            (16, 16, 0),   // `*s as i16`
        ];
        for &(from, to, decalage) in attendu {
            for echantillon in [0i32, 1, -1, 12345, -12345, 0x0034_5678, -0x0034_5678] {
                let origine = if decalage >= 0 {
                    echantillon << decalage
                } else {
                    echantillon >> (-decalage)
                };
                assert_eq!(
                    requantize(echantillon, from, to),
                    origine,
                    "requantize({echantillon}, {from}, {to}) s'écarte de la table d'origine"
                );
            }
        }
    }

    /// Le défaut lui-même : une source de 20 bits servie à la sortie locale,
    /// qui demande toujours 32 bits, sortait `2^12` fois trop bas — −72 dB.
    #[test]
    fn une_source_de_20_bits_ne_sort_plus_72_db_trop_bas() {
        // Pleine échelle sur 20 bits, droitisée : 0x0007_FFFF.
        let pleine_echelle_20 = 0x0007_FFFFi32;
        let octets = convert_pcm_bit_depth(&[pleine_echelle_20], 20, 32);
        let sorti = i32::from_le_bytes([octets[0], octets[1], octets[2], octets[3]]);

        // Le décalage de 12 rangs remet les 20 bits en haut du mot de 32.
        assert_eq!(sorti, pleine_echelle_20 << 12);

        // Formulé comme l'entend le testeur : le niveau ne doit plus être
        // divisé par 4096. Avant le correctif, `sorti` valait 0x0007_FFFF.
        let rapport = sorti as f64 / (pleine_echelle_20 << 12) as f64;
        assert!(
            (rapport - 1.0).abs() < 1e-12,
            "atténuation résiduelle : rapport {rapport}"
        );
        assert_ne!(
            sorti, pleine_echelle_20,
            "l'échantillon est resté droitisé sur 20 bits — c'est le défaut #2157"
        );
    }

    /// Le même trou en sortie 16 bits ne se contentait pas d'atténuer : il
    /// **repliait** le signal, parce qu'un mot de 20 bits ne tient pas dans un
    /// `i16` et que `*s as i16` tronque.
    #[test]
    fn une_source_de_20_bits_ne_se_replie_plus_en_16_bits() {
        let pleine_echelle_20 = 0x0007_FFFFi32; // positif, proche du maximum
        let octets = convert_pcm_bit_depth(&[pleine_echelle_20], 20, 16);
        let sorti = i16::from_le_bytes([octets[0], octets[1]]);

        // 20 -> 16 bits : quatre rangs de moins, le signe est conservé.
        assert_eq!(sorti, (pleine_echelle_20 >> 4) as i16);
        assert!(
            sorti > 0,
            "repliement : un maximum positif est ressorti {sorti}"
        );
        // L'ancien `*s as i16` rendait 0xFFFF, soit -1.
        assert_ne!(sorti, -1);
    }

    /// La normalisation qui empêche `shift`, `from_bd` et la largeur d'octets
    /// de diverger. Arrondi vers le HAUT : aucun bit source n'est perdu.
    #[test]
    fn container_bit_depth_arrondit_vers_le_conteneur_superieur() {
        assert_eq!(container_bit_depth(8), 16);
        assert_eq!(container_bit_depth(12), 16);
        assert_eq!(container_bit_depth(16), 16);
        assert_eq!(container_bit_depth(20), 24);
        assert_eq!(container_bit_depth(24), 24);
        assert_eq!(container_bit_depth(32), 32);
        // Une profondeur absurde ne doit pas produire un décalage de 32 rangs,
        // qui paniquerait.
        assert_eq!(container_bit_depth(0), 16);
        assert_eq!(container_bit_depth(64), 32);
    }

    /// `requantize` est bornée : un `bd` nul ou aberrant ne doit jamais
    /// produire un décalage >= 32, qui panique en Rust.
    #[test]
    fn requantize_ne_panique_sur_aucune_profondeur() {
        for from in [0u16, 1, 8, 12, 16, 20, 24, 32, 64, u16::MAX] {
            for to in [0u16, 1, 8, 12, 16, 20, 24, 32, 64, u16::MAX] {
                let _ = requantize(1234, from, to);
            }
        }
    }

    #[test]
    fn streaming_pcm_bytes_preserve_exact_frames_across_arbitrary_chunks() {
        let source = vec![
            0x01, 0x02, 0x03, 0x11, 0x12, 0x13, // frame 1, 24-bit stereo
            0x21, 0x22, 0x23, 0x31, 0x32, 0x33, // frame 2
        ];
        let mut adapter = StreamingPcmByteAdapter::new(24, 2, 96_000, 24, 2, 96_000)
            .expect("valid identity adapter");
        let mut output = Vec::new();
        for byte in &source {
            output.extend(adapter.push(std::slice::from_ref(byte)).unwrap());
        }
        output.extend(adapter.finish().unwrap());
        assert_eq!(output, source, "identity adaptation must be byte-for-byte");
    }

    #[test]
    fn streaming_pcm_bytes_apply_rate_channels_and_depth_to_payload() {
        let frames = 4_800usize; // 50 ms at 96 kHz -> 2 400 frames at 48 kHz.
        let mut source = Vec::with_capacity(frames * 2 * 3);
        for frame in 0..frames {
            let left = ((frame as i32 * 997) & 0x7f_ffff) - 0x40_0000;
            let right = -left;
            for sample in [left, right] {
                let bytes = sample.to_le_bytes();
                source.extend_from_slice(&bytes[..3]);
            }
        }

        let mut adapter = StreamingPcmByteAdapter::new(24, 2, 96_000, 16, 1, 48_000)
            .expect("valid conversion adapter");
        let mut output = Vec::new();
        for chunk in source.chunks(137) {
            output.extend(adapter.push(chunk).unwrap());
        }
        output.extend(adapter.finish().unwrap());

        assert_eq!(output.len() % 2, 0, "16-bit mono frames must stay aligned");
        assert_eq!(
            output.len() / 2,
            2_400,
            "rate conversion must determine the emitted frame count"
        );
    }

    #[test]
    fn streaming_pcm_bytes_refuse_a_partial_final_frame() {
        let mut adapter = StreamingPcmByteAdapter::new(24, 2, 96_000, 16, 1, 48_000)
            .expect("valid conversion adapter");
        assert!(adapter.push(&[0; 5]).unwrap().is_empty());
        let error = adapter
            .finish()
            .expect_err("five bytes are not a stereo 24-bit frame");
        assert!(error.contains("outside a complete 6-byte source frame"));
    }

    #[test]
    fn resolve_bit_depth_from_bits_per_sample() {
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = Some(24);
        assert_eq!(resolve_bit_depth(&params), 24);

        params.bits_per_sample = Some(16);
        assert_eq!(resolve_bit_depth(&params), 16);
    }

    #[test]
    fn resolve_bit_depth_from_alac_magic_cookie_24bit() {
        // Simulate an ALAC magic cookie (24 bytes): byte 5 = bit_depth = 24
        let mut cookie = vec![0u8; 24];
        cookie[5] = 24; // bit_depth field
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = None;
        params.extra_data = Some(cookie.into_boxed_slice());
        assert_eq!(resolve_bit_depth(&params), 24);
    }

    #[test]
    fn resolve_bit_depth_from_alac_magic_cookie_16bit() {
        let mut cookie = vec![0u8; 24];
        cookie[5] = 16;
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = None;
        params.extra_data = Some(cookie.into_boxed_slice());
        assert_eq!(resolve_bit_depth(&params), 16);
    }

    #[test]
    fn resolve_bit_depth_from_alac_magic_cookie_with_prefix() {
        // 48-byte cookie with frma + alac atom prefixes (12+12 = 24 prefix + 24 payload)
        let mut cookie = vec![0u8; 48];
        // frma atom at offset 4
        cookie[4..8].copy_from_slice(b"frma");
        // alac atom at offset 16
        cookie[16..20].copy_from_slice(b"alac");
        // bit_depth at byte 5 of 24-byte payload (offset 24+5=29)
        cookie[29] = 24;
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = None;
        params.extra_data = Some(cookie.into_boxed_slice());
        assert_eq!(resolve_bit_depth(&params), 24);
    }

    #[test]
    fn resolve_bit_depth_fallback_no_extra_data() {
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = None;
        params.extra_data = None;
        assert_eq!(resolve_bit_depth(&params), 16);
    }

    #[test]
    fn resolve_bit_depth_explicit_overrides_cookie() {
        // If bits_per_sample is set, extra_data is not consulted
        let mut cookie = vec![0u8; 24];
        cookie[5] = 24;
        let mut params = AudioCodecParameters::new();
        params.bits_per_sample = Some(16);
        params.extra_data = Some(cookie.into_boxed_slice());
        assert_eq!(resolve_bit_depth(&params), 16);
    }

    /// SPIKE (#1145): decode a REAL .ape fixture (Monkey's Audio, High/c3000
    /// compression, 16-bit stereo) and verify the PCM byte-for-byte matches the
    /// reference WAV that the C++ Monkey's Audio decoder produced. This is the
    /// correctness proof: it fails loudly if `ape-decoder` regresses, exactly
    /// what the old in-tree stub lacked.
    #[test]
    fn ape_decodes_real_fixture_matches_reference_wav() {
        let ape = fixture_path("ape/sine_16s_c3000.ape");
        let decoded = decode_ape_to_pcm(&ape, 0.0, 0.0).expect("ape decode");

        // Basic sanity: 44.1kHz stereo 16-bit, ~non-trivial duration, non-silent.
        assert_eq!(decoded.channels, 2, "stereo");
        assert_eq!(decoded.bit_depth, 16);
        assert!(decoded.sample_rate >= 8000, "plausible sample rate");
        assert!(decoded.duration_s > 0.1, "non-trivial duration");
        assert!(
            decoded.samples_i32.iter().any(|s| *s != 0),
            "decoded PCM must not be all-silence (the stub's failure mode)"
        );

        // Byte-for-byte compare against the C++ reference decoder output.
        let ref_wav =
            std::fs::read(fixture_path("ape/sine_16s_c3000.wav")).expect("read reference wav");
        let ref_pcm = &ref_wav[44..]; // strip 44-byte canonical WAV header
        let our_pcm = decoded.pcm_bytes();
        assert_eq!(
            our_pcm.len(),
            ref_pcm.len(),
            "PCM length must equal the reference decoder's output"
        );
        assert_eq!(
            our_pcm, ref_pcm,
            "decoded PCM must be byte-for-byte identical to the C++ reference decoder"
        );
    }
}
