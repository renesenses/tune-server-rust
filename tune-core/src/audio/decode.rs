use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;
use tokio::sync::mpsc;
use tracing::{debug, error};

use super::dsd_to_pcm::{DsdToPcmConverter, choose_output_rate};

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

pub fn can_decode_native(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "flac"
            | "mp3"
            | "wav"
            | "m4a"
            | "aac"
            | "alac"
            | "ogg"
            | "aiff"
            | "aif"
            | "dsf"
            | "dff"
            | "wv"
            | "ape"
    )
}

fn is_wavpack(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    ext == "wv"
}

pub fn decode_to_pcm(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "aiff" || ext == "aif" {
        return super::aiff::decode_aiff_to_pcm(file_path, seek_s, max_duration_s);
    }

    if ext == "dsf" || ext == "dff" {
        return decode_dsd_to_pcm(
            file_path,
            &ext,
            target_sample_rate,
            target_channels,
            seek_s,
            max_duration_s,
        );
    }

    if is_wavpack(file_path) {
        return super::wavpack::decode_wavpack_to_pcm(
            file_path,
            target_sample_rate,
            target_channels,
            seek_s,
            max_duration_s,
        );
    }

    if ext == "ape" {
        // Wrap in catch_unwind: the native APE decoder may panic on malformed
        // or unsupported APE files (e.g. very old versions, Insane compression).
        // A panic must NOT crash the server.
        let fp = file_path.to_string();
        let result = catch_unwind(AssertUnwindSafe(move || {
            super::ape::decode_ape_to_pcm(
                &fp,
                target_sample_rate,
                target_channels,
                seek_s,
                max_duration_s,
            )
        }));
        return match result {
            Ok(inner) => inner,
            Err(panic_info) => {
                let msg = panic_payload_to_string(&panic_info);
                error!(file = file_path, panic = %msg, "ape_decoder_panic");
                Err(format!("APE decode panic: {msg}"))
            }
        };
    }

    // Wrap symphonia decode in catch_unwind — an unsupported codec or
    // malformed file must never panic-crash the server.
    let fp = file_path.to_string();
    let result = catch_unwind(AssertUnwindSafe(move || {
        decode_symphonia(
            &fp,
            target_sample_rate,
            target_channels,
            seek_s,
            max_duration_s,
        )
    }));
    match result {
        Ok(inner) => inner,
        Err(panic_info) => {
            let msg = panic_payload_to_string(&panic_info);
            error!(file = file_path, panic = %msg, "symphonia_decoder_panic");
            Err(format!("decode panic ({ext}): {msg}"))
        }
    }
}

/// Streaming decode: decodes a file packet-by-packet and sends PCM chunks
/// progressively through the provided channel. This allows the HTTP stream
/// handler to start serving data to the DLNA renderer immediately, without
/// waiting for the entire file to be decoded.
///
/// Returns the source bit depth on success so the caller can set up headers.
/// For non-symphonia formats (AIFF, DSD, WavPack, APE), falls back to full
/// decode + chunked send (still benefits from the early session creation).
pub fn decode_to_pcm_streaming(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
) -> Result<u16, String> {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Non-symphonia formats: fall back to full decode then stream chunks.
    // This still benefits from the session being created early.
    if matches!(ext.as_str(), "aiff" | "aif" | "dsf" | "dff" | "wv" | "ape") {
        let decoded = decode_to_pcm(file_path, target_sample_rate, target_channels, 0.0, 0.0)?;
        let pcm_bytes = decoded.pcm_bytes();
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for streaming decode")?;
        for chunk in pcm_bytes.chunks(chunk_size) {
            if rt.block_on(tx.send(chunk.to_vec())).is_err() {
                debug!("streaming_decode_consumer_dropped (fallback)");
                return Ok(decoded.bit_depth);
            }
        }
        return Ok(decoded.bit_depth);
    }

    // Symphonia streaming decode: packet-by-packet progressive output
    let file = File::open(file_path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

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
    let track_id = track.id;
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let source_bd = resolve_bit_depth(&audio_params);
    let shift = 32u16.saturating_sub(source_bd);

    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "no tokio runtime for streaming decode")?;

    // Accumulate PCM bytes and flush when exceeding chunk_size.
    // This avoids sending tiny per-packet buffers over the channel.
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(chunk_size * 2);
    let mut total_samples: usize = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
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

        // Normalize: right-justify samples (same as batch decode)
        if shift > 0 && shift < 32 {
            for s in packet_samples.iter_mut() {
                *s >>= shift;
            }
        }

        total_samples += packet_samples.len();

        // Convert to PCM bytes based on bit depth
        match source_bd {
            24 => {
                for s in &packet_samples {
                    let b = s.to_le_bytes();
                    pcm_buf.extend_from_slice(&[b[0], b[1], b[2]]);
                }
            }
            32 => {
                for s in &packet_samples {
                    pcm_buf.extend_from_slice(&s.to_le_bytes());
                }
            }
            _ => {
                // 16-bit
                for s in &packet_samples {
                    pcm_buf.extend_from_slice(&(*s as i16).to_le_bytes());
                }
            }
        }

        // Flush accumulated buffer when it exceeds chunk_size
        while pcm_buf.len() >= chunk_size {
            let chunk: Vec<u8> = pcm_buf.drain(..chunk_size).collect();
            if rt.block_on(tx.send(chunk)).is_err() {
                debug!("streaming_decode_consumer_dropped");
                return Ok(source_bd);
            }
        }
    }

    // Flush remaining bytes
    if !pcm_buf.is_empty() {
        if rt.block_on(tx.send(pcm_buf)).is_err() {
            debug!("streaming_decode_consumer_dropped (final)");
        }
    }

    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let total_frames = total_samples as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = file_path,
        samples = total_samples,
        rate = source_rate,
        channels = source_channels,
        duration_s,
        "decoded_symphonia_streaming"
    );

    Ok(source_bd)
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

/// Symphonia-based decoder for standard formats (FLAC, MP3, WAV, M4A, OGG, etc).
fn decode_symphonia(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let file = File::open(file_path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

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
    let track_id = track.id;
    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Seek if requested
    if seek_s > 0.0 {
        let seconds = seek_s as i64;
        let nanos = ((seek_s - seconds as f64) * 1_000_000_000.0) as u32;
        let time = Time::try_new(seconds, nanos).unwrap_or(Time::ZERO);
        let _ = format.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        );
    }

    let source_bd = resolve_bit_depth(&audio_params);

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

    let out_rate = target_sample_rate.unwrap_or(source_rate);
    let out_channels = target_channels.unwrap_or(source_channels);
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

/// Decode a DSD file (DSF or DFF) to PCM using native parsers.
fn decode_dsd_to_pcm(
    file_path: &str,
    ext: &str,
    target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let (dsd_rate, channels, dsd_data) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let data = super::dsf::read_dsf_blocks(file_path, &info)?;
        (info.sample_rate, info.channels as usize, data)
    } else {
        let info = super::dff::parse_dff(file_path)?;
        let data = super::dff::read_dff_data(file_path, &info)?;
        (info.sample_rate, info.channels as usize, data)
    };

    let lsb_first = ext == "dsf";
    let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));

    let converter = DsdToPcmConverter::new(dsd_rate, output_rate, channels, lsb_first);

    // Use native 24-bit output from the DSD converter.  The converter
    // produces 3 bytes per sample (24-bit LE); we reconstruct i32 values
    // from those triplets so the rest of the pipeline (pcm_bytes, WAV
    // header, DLNA streaming) all agree on 24-bit depth.
    //
    // Previously this used process_to_i16() which truncated to 16-bit,
    // but the orchestrator declared bit_depth=24 for DSD in the WAV
    // header.  The mismatch (header says 24-bit / data is 16-bit)
    // caused malformed WAV streams that DLNA renderers such as the
    // Marantz SR7009 could not play.
    let pcm_24 = converter.process(&dsd_data);
    let num_samples = pcm_24.len() / 3;
    let mut all_samples: Vec<i32> = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let offset = i * 3;
        let lo = pcm_24[offset] as u32;
        let mid = pcm_24[offset + 1] as u32;
        let hi = pcm_24[offset + 2] as u32;
        let val24 = lo | (mid << 8) | (hi << 16);
        // Sign-extend from 24-bit to 32-bit
        let val32 = if val24 & 0x80_0000 != 0 {
            (val24 | 0xFF00_0000) as i32
        } else {
            val24 as i32
        };
        all_samples.push(val32);
    }

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
    let end = (start + max_samples).min(all_samples.len());
    let trimmed = &all_samples[start..end];

    let actual_frames = trimmed.len() / channels;
    let actual_duration = actual_frames as f64 / output_rate as f64;

    debug!(
        file = file_path,
        ext,
        dsd_rate,
        output_rate,
        channels,
        samples = trimmed.len(),
        duration_s = actual_duration,
        "decoded_dsd_native"
    );

    Ok(DecodedAudio {
        samples_i32: trimmed.to_vec(),
        bit_depth: 24,
        sample_rate: output_rate,
        channels: channels as u32,
        duration_s: actual_duration,
    })
}

#[cfg(test)]
mod decode_integration_tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> String {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures");
        p.push(name);
        p.to_string_lossy().to_string()
    }

    #[test]
    fn can_decode_native_formats() {
        assert!(can_decode_native("song.flac"));
        assert!(can_decode_native("song.mp3"));
        assert!(can_decode_native("song.wav"));
        assert!(can_decode_native("song.m4a"));
        assert!(can_decode_native("song.ogg"));
        assert!(can_decode_native("song.aiff"));
        assert!(can_decode_native("song.aif"));
        assert!(can_decode_native("song.dsf"));
        assert!(can_decode_native("song.dff"));
        assert!(can_decode_native("song.ape"));
        assert!(can_decode_native("song.wv"));
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
        let path = fixture_path("test.ogg");
        let result = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert!(!result.samples_i32.is_empty(), "OGG should produce samples");
        assert_eq!(result.sample_rate, 44100);
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
    fn dsf_is_native() {
        assert!(can_decode_native("test.dsf"));
        assert!(can_decode_native("test.dff"));
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
}
