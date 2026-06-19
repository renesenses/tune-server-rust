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

use super::dsd_to_pcm::choose_output_rate;

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

/// Convert right-justified i32 samples from one bit depth to another,
/// producing raw PCM bytes at the target depth.
///
/// Source samples are assumed to be right-justified (i.e. a 24-bit sample
/// occupies bits 0..23 of the i32, a 16-bit sample occupies bits 0..15).
fn convert_pcm_bit_depth(samples: &[i32], from_bd: u16, to_bd: u16) -> Vec<u8> {
    match to_bd {
        24 => samples
            .iter()
            .map(|s| {
                let v = match from_bd {
                    32 => *s >> 8,
                    16 => (*s as i32) << 8,
                    _ => *s,
                };
                let b = v.to_le_bytes();
                [b[0], b[1], b[2]]
            })
            .flat_map(|a| a.into_iter())
            .collect(),
        32 => samples
            .iter()
            .map(|s| {
                let v = match from_bd {
                    24 => *s << 8,
                    16 => (*s as i32) << 16,
                    _ => *s,
                };
                v.to_le_bytes()
            })
            .flat_map(|a| a.into_iter())
            .collect(),
        _ => {
            // 16-bit output
            samples
                .iter()
                .flat_map(|s| {
                    let v = match from_bd {
                        32 => (*s >> 16) as i16,
                        24 => (*s >> 8) as i16,
                        _ => *s as i16,
                    };
                    v.to_le_bytes()
                })
                .collect()
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

    if ext == "wma" || ext == "asf" {
        return decode_wma_via_ffmpeg(
            file_path,
            target_sample_rate,
            target_channels,
            seek_s,
            max_duration_s,
        );
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
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        None,
        tx,
        chunk_size,
        None,
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
) -> Result<u16, String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        None,
        tx,
        chunk_size,
        Some(data_ready),
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
    levels_tx: tokio::sync::mpsc::UnboundedSender<super::levels::AudioLevels>,
) -> Result<u16, String> {
    decode_to_pcm_streaming_inner(
        file_path,
        target_sample_rate,
        target_channels,
        target_bit_depth,
        tx,
        chunk_size,
        Some(data_ready),
        Some(levels_tx),
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
    levels_tx: Option<tokio::sync::mpsc::UnboundedSender<super::levels::AudioLevels>>,
) -> Result<u16, String> {
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

        return decode_dsd_streaming(
            file_path,
            &ext,
            target_sample_rate,
            output_bd,
            tx,
            chunk_size,
            &mut first_chunk_sent,
            &data_ready,
            &levels_tx,
            &rt,
        );
    }

    // Non-symphonia formats: fall back to full decode then stream chunks.
    // This still benefits from the session being created early.
    if matches!(ext.as_str(), "aiff" | "aif" | "wv" | "ape" | "wma" | "asf") {
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
        let ch = target_channels.unwrap_or(decoded.channels as u32) as u16;
        for chunk in pcm_bytes.chunks(chunk_size) {
            // Send PCM data first, compute levels after (same rationale
            // as the symphonia path: avoid delaying the audio stream).
            if rt.block_on(tx.send(chunk.to_vec())).is_err() {
                debug!("streaming_decode_consumer_dropped (fallback)");
                return Ok(output_bd);
            }
            if !first_chunk_sent {
                first_chunk_sent = true;
                if let Some(ref n) = data_ready {
                    n.notify_one();
                }
            }
            if let Some(ref ltx) = levels_tx {
                let sr = target_sample_rate.unwrap_or(decoded.sample_rate);
                let _ = ltx.send(super::levels::compute_levels(chunk, output_bd, ch, sr));
            }
        }
        return Ok(output_bd);
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

    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let source_bd = resolve_bit_depth(&audio_params);
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

        // Convert to PCM bytes based on output bit depth.
        // When target_bit_depth differs from source_bd, the samples are
        // already right-justified to source_bd range, so we re-quantize
        // to the output depth (e.g. 32-bit source -> 24-bit output by
        // taking the upper 24 bits, or 24-bit source -> 16-bit output
        // by truncating to the upper 16 bits).
        match output_bd {
            24 => {
                for s in &packet_samples {
                    // If source is 32-bit, shift right by 8 to get upper 24 bits
                    let v = if source_bd == 32 { *s >> 8 } else { *s };
                    let b = v.to_le_bytes();
                    pcm_buf.extend_from_slice(&[b[0], b[1], b[2]]);
                }
            }
            32 => {
                for s in &packet_samples {
                    // If source is 24-bit or 16-bit, shift left to fill 32-bit range
                    let v = if source_bd == 24 {
                        *s << 8
                    } else if source_bd == 16 {
                        (*s as i32) << 16
                    } else {
                        *s
                    };
                    pcm_buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            _ => {
                // 16-bit output
                for s in &packet_samples {
                    // If source is 24-bit, shift right by 8; if 32-bit, by 16
                    let v = if source_bd == 32 {
                        (*s >> 16) as i16
                    } else if source_bd == 24 {
                        (*s >> 8) as i16
                    } else {
                        *s as i16
                    };
                    pcm_buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        }

        while pcm_buf.len() >= chunk_size {
            let chunk: Vec<u8> = pcm_buf.drain(..chunk_size).collect();
            // Send PCM data FIRST to avoid delaying the audio stream.
            // compute_levels() is CPU-intensive (iterates all frames with
            // floating-point math) and was previously called before send(),
            // introducing micro-pauses that caused Squeezebox/LMS stuttering.
            if rt.block_on(tx.send(chunk.clone())).is_err() {
                debug!("streaming_decode_consumer_dropped");
                return Ok(output_bd);
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
                let _ = ltx.send(super::levels::compute_levels(
                    &chunk,
                    output_bd,
                    source_channels as u16,
                    source_rate,
                ));
            }
        }
    }

    // Flush remaining bytes
    if !pcm_buf.is_empty() {
        if rt.block_on(tx.send(pcm_buf)).is_err() {
            debug!("streaming_decode_consumer_dropped (final)");
        }
    }

    let total_frames = total_samples as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = file_path,
        samples = total_samples,
        rate = source_rate,
        channels = source_channels,
        source_bd,
        output_bd,
        duration_s,
        "decoded_symphonia_streaming"
    );

    Ok(output_bd)
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

/// Decode a WMA/ASF file to PCM via FFmpeg subprocess.
///
/// Symphonia does not support the WMA codec or ASF container, so we shell out
/// to FFmpeg which handles all WMA variants (WMA v1/v2/Pro/Lossless).
/// FFmpeg outputs raw signed 16-bit little-endian PCM to stdout.
fn decode_wma_via_ffmpeg(
    file_path: &str,
    _target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    use std::process::Command;

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error"]);

    if seek_s > 0.0 {
        cmd.args(["-ss", &format!("{seek_s}")]);
    }

    cmd.args(["-i", file_path]);

    if max_duration_s > 0.0 {
        cmd.args(["-t", &format!("{max_duration_s}")]);
    }

    // Probe the file first to get sample rate and channels
    let probe_output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=sample_rate,channels,bits_per_raw_sample",
            "-of",
            "csv=p=0",
            file_path,
        ])
        .output()
        .map_err(|e| format!("ffprobe failed (is ffmpeg installed?): {e}"))?;

    if !probe_output.status.success() {
        let stderr = String::from_utf8_lossy(&probe_output.stderr);
        return Err(format!("ffprobe error: {stderr}"));
    }

    let probe_str = String::from_utf8_lossy(&probe_output.stdout);
    let parts: Vec<&str> = probe_str.trim().split(',').collect();
    let source_rate: u32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(44100);
    let source_channels: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(2);
    // WMA is typically 16-bit; WMA Lossless can be 24-bit
    let source_bd: u16 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
    let out_bd = if source_bd >= 24 { 24u16 } else { 16u16 };

    let pcm_fmt = if out_bd == 24 { "s24le" } else { "s16le" };
    cmd.args([
        "-f",
        pcm_fmt,
        "-acodec",
        &format!("pcm_{pcm_fmt}"),
        "-ar",
        &source_rate.to_string(),
        "-ac",
        &source_channels.to_string(),
        "-",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| format!("ffmpeg failed (is ffmpeg installed?): {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg WMA decode error: {stderr}"));
    }

    let pcm_data = output.stdout;
    let bytes_per_sample = if out_bd == 24 { 3 } else { 2 };
    let num_samples = pcm_data.len() / bytes_per_sample;

    let mut samples: Vec<i32> = Vec::with_capacity(num_samples);
    if out_bd == 24 {
        for i in 0..num_samples {
            let offset = i * 3;
            if offset + 2 >= pcm_data.len() {
                break;
            }
            let lo = pcm_data[offset] as u32;
            let mid = pcm_data[offset + 1] as u32;
            let hi = pcm_data[offset + 2] as u32;
            let val24 = lo | (mid << 8) | (hi << 16);
            let val32 = if val24 & 0x80_0000 != 0 {
                (val24 | 0xFF00_0000) as i32
            } else {
                val24 as i32
            };
            samples.push(val32);
        }
    } else {
        for i in 0..num_samples {
            let offset = i * 2;
            if offset + 1 >= pcm_data.len() {
                break;
            }
            let val = i16::from_le_bytes([pcm_data[offset], pcm_data[offset + 1]]);
            samples.push(val as i32);
        }
    }

    let total_frames = samples.len() as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = file_path,
        samples = samples.len(),
        rate = source_rate,
        channels = source_channels,
        bit_depth = out_bd,
        duration_s,
        "decoded_wma_ffmpeg"
    );

    Ok(DecodedAudio {
        samples_i32: samples,
        bit_depth: out_bd,
        sample_rate: source_rate,
        channels: source_channels,
        duration_s,
    })
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
    output_bd: u16,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    first_chunk_sent: &mut bool,
    data_ready: &Option<std::sync::Arc<tokio::sync::Notify>>,
    levels_tx: &Option<tokio::sync::mpsc::UnboundedSender<super::levels::AudioLevels>>,
    rt: &tokio::runtime::Handle,
) -> Result<u16, String> {
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

    // Accumulate PCM output and flush in chunk_size batches
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(chunk_size * 2);
    let ch = channels as u16;

    // Inner loop: feed DSD chunks, convert PCM, send downstream.
    // Factored into a closure to avoid duplicating the flush logic.
    let mut process_dsd_chunk =
        |streamer: &mut DsdToPcmStreamer, dsd_chunk: &[u8]| -> Result<bool, String> {
            let pcm_24 = streamer.feed(dsd_chunk);
            if pcm_24.is_empty() {
                return Ok(false);
            }
            let converted = convert_24bit_pcm_to_depth(&pcm_24, output_bd);
            pcm_buf.extend_from_slice(&converted);
            while pcm_buf.len() >= chunk_size {
                let chunk: Vec<u8> = pcm_buf.drain(..chunk_size).collect();
                // Send PCM data first, compute levels after (same rationale
                // as the symphonia path: avoid delaying the audio stream).
                if rt.block_on(tx.send(chunk.clone())).is_err() {
                    debug!("dsd_streaming_consumer_dropped");
                    return Ok(true); // consumer gone
                }
                if !*first_chunk_sent {
                    *first_chunk_sent = true;
                    if let Some(n) = data_ready {
                        n.notify_one();
                    }
                }
                if let Some(ltx) = levels_tx {
                    let _ = ltx.send(super::levels::compute_levels(
                        &chunk,
                        output_bd,
                        ch,
                        output_rate,
                    ));
                }
            }
            Ok(false)
        };

    // Read and process DSD data in chunks
    if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let mut reader = super::dsf::DsfStreamReader::open(file_path, info)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_dsd_chunk(&mut streamer, &dsd_chunk)? {
                return Ok(output_bd);
            }
        }
    } else {
        let info = super::dff::parse_dff(file_path)?;
        // Read in chunks aligned to channel count.
        // 32768 bytes is a good balance: small enough for low memory, large
        // enough to amortize I/O overhead.
        let read_chunk = 32768 / channels * channels;
        let mut reader = super::dff::DffStreamReader::open(file_path, &info, read_chunk)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            if process_dsd_chunk(&mut streamer, &dsd_chunk)? {
                return Ok(output_bd);
            }
        }
    }

    // Flush remaining samples from the FIR filter
    let tail = streamer.flush();
    if !tail.is_empty() {
        let converted = convert_24bit_pcm_to_depth(&tail, output_bd);
        pcm_buf.extend_from_slice(&converted);
    }

    // Send any remaining bytes (send first, levels after)
    if !pcm_buf.is_empty() {
        if rt.block_on(tx.send(pcm_buf.clone())).is_err() {
            debug!("dsd_streaming_consumer_dropped (final)");
        } else if let Some(ltx) = levels_tx {
            let _ = ltx.send(super::levels::compute_levels(
                &pcm_buf,
                output_bd,
                ch,
                output_rate,
            ));
        }
    }

    let total_samples = streamer.total_output_samples();
    let total_frames = total_samples as f64 / channels as f64;
    let duration_s = total_frames / output_rate as f64;

    debug!(
        file = file_path,
        ext, dsd_rate, output_rate, channels, total_samples, duration_s, "decoded_dsd_streaming"
    );

    Ok(output_bd)
}

/// Convert 24-bit LE PCM byte triples to the target bit depth.
fn convert_24bit_pcm_to_depth(pcm_24: &[u8], target_bd: u16) -> Vec<u8> {
    if target_bd == 24 {
        return pcm_24.to_vec();
    }

    let num_samples = pcm_24.len() / 3;
    match target_bd {
        16 => {
            let mut out = Vec::with_capacity(num_samples * 2);
            for i in 0..num_samples {
                let offset = i * 3;
                // Take upper 16 bits of 24-bit value (bytes [1] and [2])
                out.push(pcm_24[offset + 1]);
                out.push(pcm_24[offset + 2]);
            }
            out
        }
        32 => {
            let mut out = Vec::with_capacity(num_samples * 4);
            for i in 0..num_samples {
                let offset = i * 3;
                // Shift left by 8 to fill 32-bit range
                out.push(0); // LSB padding
                out.push(pcm_24[offset]);
                out.push(pcm_24[offset + 1]);
                out.push(pcm_24[offset + 2]);
            }
            out
        }
        _ => pcm_24.to_vec(), // fallback: keep 24-bit
    }
}

/// Decode a DSD file (DSF or DFF) to PCM using streaming converter.
///
/// Uses `DsdToPcmStreamer` to process the file in chunks, avoiding the
/// catastrophic memory usage of the old batch approach.
/// Memory usage: O(block_size + filter_len) ≈ 40 KB regardless of file size.
fn decode_dsd_to_pcm(
    file_path: &str,
    ext: &str,
    target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    use super::dsd_to_pcm::DsdToPcmStreamer;

    // Process the file in chunks using the streaming readers.
    // Parse header once, then create both the streamer and the reader from it.
    let mut all_pcm_24: Vec<u8> = Vec::new();

    let (dsd_rate, output_rate, channels) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(file_path)?;
        let dsd_rate = info.sample_rate;
        let channels = info.channels as usize;
        let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));
        let mut streamer = DsdToPcmStreamer::new(dsd_rate, output_rate, channels, true);
        let mut reader = super::dsf::DsfStreamReader::open(file_path, info)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            all_pcm_24.extend_from_slice(&streamer.feed(&dsd_chunk));
        }
        all_pcm_24.extend_from_slice(&streamer.flush());
        (dsd_rate, output_rate, channels)
    } else {
        let info = super::dff::parse_dff(file_path)?;
        let dsd_rate = info.sample_rate;
        let channels = info.channels as usize;
        let output_rate = target_sample_rate.unwrap_or_else(|| choose_output_rate(dsd_rate));
        let mut streamer = DsdToPcmStreamer::new(dsd_rate, output_rate, channels, false);
        let read_chunk = 32768 / channels * channels;
        let mut reader = super::dff::DffStreamReader::open(file_path, &info, read_chunk)?;
        while let Some(dsd_chunk) = reader.next_chunk()? {
            all_pcm_24.extend_from_slice(&streamer.feed(&dsd_chunk));
        }
        all_pcm_24.extend_from_slice(&streamer.flush());
        (dsd_rate, output_rate, channels)
    };

    // Convert 24-bit LE bytes to i32 samples
    let num_samples = all_pcm_24.len() / 3;
    let mut all_samples: Vec<i32> = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let offset = i * 3;
        let lo = all_pcm_24[offset] as u32;
        let mid = all_pcm_24[offset + 1] as u32;
        let hi = all_pcm_24[offset + 2] as u32;
        let val24 = lo | (mid << 8) | (hi << 16);
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
        assert!(!can_decode_native("song.wma")); // WMA requires external FFmpeg
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
