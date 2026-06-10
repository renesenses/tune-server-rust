use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, calculate_cutoff,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, TransportState};

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub is_default: bool,
    pub max_channels: u16,
    pub sample_rates: Vec<u32>,
}

pub fn list_audio_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let host_name = host.id().name();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
        .unwrap_or_default();

    info!(
        host = %host_name,
        default_device = %default_name,
        "local_audio_enumerating_devices"
    );

    let mut devices = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    match host.output_devices() {
        Ok(output_devices) => {
            for device in output_devices {
                let name = device
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "Unknown".into());

                // Skip duplicate device names — cpal/CoreAudio can report the
                // same physical device multiple times (e.g. with different
                // stream configurations).  We keep the first occurrence.
                if !seen_names.insert(name.clone()) {
                    debug!(device = %name, "local_audio_device_skipped_duplicate");
                    continue;
                }

                let is_default = name == default_name;

                let (max_channels, sample_rates) = match device.supported_output_configs() {
                    Ok(configs) => {
                        let mut max_ch = 0u16;
                        let mut rates = Vec::new();
                        for config in configs {
                            max_ch = max_ch.max(config.channels());
                            let min = config.min_sample_rate();
                            let max = config.max_sample_rate();
                            for &rate in
                                &[44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000]
                            {
                                if rate >= min && rate <= max && !rates.contains(&rate) {
                                    rates.push(rate);
                                }
                            }
                        }
                        rates.sort();

                        // PipeWire's ALSA plugin can return Ok but with an
                        // empty iterator — treat it like an error and fall
                        // through to the fallback probe below.
                        if max_ch == 0 || rates.is_empty() {
                            debug!(
                                device = %name,
                                "local_audio_device_supported_configs_empty"
                            );
                            probe_device_fallback_caps(&device, &name)
                        } else {
                            (max_ch, rates)
                        }
                    }
                    Err(_) => {
                        debug!(
                            device = %name,
                            "local_audio_device_supported_configs_failed"
                        );
                        probe_device_fallback_caps(&device, &name)
                    }
                };

                info!(
                    device = %name,
                    is_default,
                    max_channels,
                    sample_rates = ?sample_rates,
                    "local_audio_device_found"
                );

                devices.push(AudioDevice {
                    name,
                    is_default,
                    max_channels,
                    sample_rates,
                });
            }
        }
        Err(e) => {
            warn!(error = %e, host = %host_name, "local_audio_output_devices_enumeration_failed");
        }
    }

    if devices.is_empty() {
        log_no_devices_diagnostics(&host_name);
    } else {
        info!(count = devices.len(), "local_audio_devices_enumerated");
    }

    devices
}

/// Log detailed diagnostics when zero audio devices are found.
///
/// On Linux, checks for PipeWire and provides actionable guidance.
/// On other platforms, logs a simple warning.
fn log_no_devices_diagnostics(host_name: &str) {
    #[cfg(target_os = "linux")]
    {
        // Check if PipeWire is running (it provides ALSA compat layer)
        let pipewire_active = std::fs::read_to_string("/run/user/1000/pipewire-0").is_ok()
            || std::process::Command::new("pgrep")
                .args(["-x", "pipewire"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        // Check if PulseAudio compat is running
        let pulseaudio_active = std::process::Command::new("pgrep")
            .args(["-x", "pipewire-pulse"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || std::process::Command::new("pgrep")
                .args(["-x", "pulseaudio"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        // Check if ALSA devices are visible at kernel level
        let proc_asound_cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        let kernel_cards: Vec<&str> = proc_asound_cards
            .lines()
            .filter(|l| l.contains('['))
            .collect();

        // Check if libasound is available
        let libasound_ok = std::path::Path::new("/usr/lib/x86_64-linux-gnu/libasound.so.2")
            .exists()
            || std::path::Path::new("/usr/lib/aarch64-linux-gnu/libasound.so.2").exists()
            || std::path::Path::new("/usr/lib/libasound.so.2").exists();

        // Check ALSA config for PipeWire PCM plugin
        let alsa_conf_has_pipewire =
            std::fs::read_to_string("/etc/alsa/conf.d/99-pipewire-default.conf")
                .or_else(|_| {
                    std::fs::read_to_string("/usr/share/alsa/alsa.conf.d/99-pipewire-default.conf")
                })
                .or_else(|_| {
                    std::fs::read_to_string("/usr/share/alsa/alsa.conf.d/50-pipewire.conf")
                })
                .map(|c| c.contains("pipewire"))
                .unwrap_or(false);

        warn!(
            host = %host_name,
            pipewire_active,
            pulseaudio_compat_active = pulseaudio_active,
            kernel_sound_cards = kernel_cards.len(),
            libasound_available = libasound_ok,
            alsa_pipewire_plugin = alsa_conf_has_pipewire,
            "local_audio_no_output_devices_found — \
             if PipeWire is active, ensure pipewire-alsa is installed \
             (provides the ALSA PCM plugin so cpal can see devices). \
             Install: sudo apt install pipewire-alsa (Debian/Ubuntu) \
             or pipewire-alsa (Fedora/Arch). \
             Also verify: aplay -l shows devices, \
             /proc/asound/cards lists sound cards."
        );

        if !kernel_cards.is_empty() {
            info!(
                cards = ?kernel_cards,
                "local_audio_kernel_sound_cards_detected — \
                 kernel sees sound hardware but cpal ({host_name}) returned zero devices"
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn!(
            host = %host_name,
            "local_audio_no_output_devices_found"
        );
    }
}

// ---------------------------------------------------------------------------
// LocalOutput — streams audio from an HTTP URL to a local audio device
// ---------------------------------------------------------------------------

pub struct LocalOutput {
    device_name: String,
    device_id: String,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    /// Volume stored before mute, so unmute can restore it
    pre_mute_volume: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    /// Playback position in milliseconds (updated by the streaming thread)
    position_ms: Arc<AtomicU64>,
    /// Track duration in milliseconds
    duration_ms: Arc<AtomicU64>,
    current_uri: Arc<std::sync::Mutex<Option<String>>>,
    track_title: Arc<std::sync::Mutex<Option<String>>>,
    track_artist: Arc<std::sync::Mutex<Option<String>>>,
    stop_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Handle to the playback thread so `stop()` can wait for it to exit.
    play_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// When true (and on macOS), use CoreAudio exclusive/hog mode for
    /// bit-perfect output, bypassing the system mixer.
    exclusive_mode: bool,
    /// Set by stop() to immediately silence the cpal callback, even if
    /// the playback thread hasn't exited yet.  Prevents overlapping audio
    /// when switching tracks and the old thread is still draining.
    ///
    /// IMPORTANT: This is replaced with a fresh Arc on each new play_url()
    /// call, so that resetting it to `false` for the new stream does NOT
    /// accidentally un-silence the old stream's callback (which keeps its
    /// own clone of the previous Arc).
    force_silent: std::sync::Mutex<Arc<AtomicBool>>,
}

impl LocalOutput {
    pub fn new(device_name: String) -> Self {
        Self::new_with_exclusive(device_name, false)
    }

    /// Create a new `LocalOutput` with explicit exclusive-mode control.
    ///
    /// When `exclusive_mode` is `true` and the platform is macOS, playback
    /// claims hog mode on the device and sets the hardware sample rate / bit
    /// depth to match the source, bypassing the system audio mixer.
    pub fn new_with_exclusive(device_name: String, exclusive_mode: bool) -> Self {
        let device_id = format!("local:{device_name}");
        Self {
            device_name,
            device_id,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1000)),
            pre_mute_volume: Arc::new(AtomicU32::new(1000)),
            muted: Arc::new(AtomicBool::new(false)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(std::sync::Mutex::new(None)),
            track_title: Arc::new(std::sync::Mutex::new(None)),
            track_artist: Arc::new(std::sync::Mutex::new(None)),
            stop_tx: std::sync::Mutex::new(None),
            play_thread: std::sync::Mutex::new(None),
            exclusive_mode,
            force_silent: std::sync::Mutex::new(Arc::new(AtomicBool::new(false))),
        }
    }

    /// Returns `true` if exclusive/bit-perfect mode is supported on this platform.
    pub fn supports_exclusive_mode() -> bool {
        #[cfg(target_os = "macos")]
        {
            true
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

/// Ring buffer shared between the HTTP reader thread and the audio callback.
///
/// Also used by `coreaudio_exclusive` on macOS for bit-perfect output.
pub struct RingBuf {
    buf: Box<[f32]>,
    /// Write position (HTTP thread writes here)
    write: AtomicU64,
    /// Read position (audio callback reads here)
    read: AtomicU64,
}

impl RingBuf {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0f32; capacity].into_boxed_slice(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of samples available to read
    pub fn available(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        (w.wrapping_sub(r)) as usize
    }

    /// Push samples into the ring buffer. Returns number actually written.
    pub fn push(&self, samples: &[f32]) -> usize {
        let cap = self.capacity();
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let free = cap - (w.wrapping_sub(r)) as usize;
        let n = samples.len().min(free);
        for i in 0..n {
            let idx = (w as usize + i) % cap;
            // Safety: single writer thread, index always in bounds
            unsafe {
                let ptr = self.buf.as_ptr() as *mut f32;
                *ptr.add(idx) = samples[i];
            }
        }
        self.write.store(w + n as u64, Ordering::Release);
        n
    }

    /// Read samples from the ring buffer into `out`. Returns number actually read.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let avail = (w.wrapping_sub(r)) as usize;
        let n = out.len().min(avail);
        let cap = self.capacity();
        for i in 0..n {
            let idx = (r as usize + i) % cap;
            out[i] = self.buf[idx];
        }
        self.read.store(r + n as u64, Ordering::Release);
        n
    }
}

/// Decode a compressed audio stream (FLAC, MP3, AAC, etc.) into f32 samples using symphonia.
/// Returns (channels, sample_rate, samples) or None if decoding fails.
fn decode_compressed_stream(data: &[u8]) -> Option<(u16, u32, Vec<f32>)> {
    use std::io::Cursor;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let hint = Hint::new();

    let mut format: Box<dyn symphonia::core::formats::FormatReader> =
        symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .ok()?;

    let track = format.default_track(TrackType::Audio)?;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return None,
    };
    let track_id = track.id;
    let sample_rate = audio_params.sample_rate.unwrap_or(44100);
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .ok()?;

    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Convert decoded audio to interleaved f32 samples
        let mut packet_samples: Vec<f32> = Vec::new();
        decoded.copy_to_vec_interleaved::<f32>(&mut packet_samples);
        all_samples.extend_from_slice(&packet_samples);
    }

    if all_samples.is_empty() {
        return None;
    }

    info!(
        channels,
        sample_rate,
        samples = all_samples.len(),
        "local_audio_decoded_compressed_stream"
    );

    Some((channels, sample_rate, all_samples))
}

/// WAV format tag constants.
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Parse a WAV header and return (channels, sample_rate, bit_depth, data_offset).
///
/// Handles PCM (format tag 1), IEEE Float (3), and WAVE_FORMAT_EXTENSIBLE
/// (0xFFFE).  For EXTENSIBLE, the actual sub-format is checked and
/// `wValidBitsPerSample` is used instead of the container size.
///
/// The `bit_depth` returned is the *effective* bit depth for PCM
/// interpretation:
///   - PCM integer: `wBitsPerSample` (or `wValidBitsPerSample` for EXTENSIBLE)
///   - IEEE Float 32-bit: returns 0 as a sentinel so `pcm_bytes_to_f32`
///     uses the float path.
fn parse_wav_header(header: &[u8]) -> Option<(u16, u32, u16, usize)> {
    if header.len() < 44 {
        return None;
    }
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }

    // Walk chunks to find "fmt " and "data"
    let mut offset = 12;
    let mut channels = 2u16;
    let mut sample_rate = 44100u32;
    let mut bit_depth = 16u16;
    let mut data_offset = None;

    while offset + 8 <= header.len() {
        let chunk_id = &header[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            header[offset + 4],
            header[offset + 5],
            header[offset + 6],
            header[offset + 7],
        ]) as usize;

        if chunk_id == b"fmt " && offset + 8 + chunk_size <= header.len() {
            let fmt = &header[offset + 8..];
            let format_tag = u16::from_le_bytes([fmt[0], fmt[1]]);
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            let block_align = u16::from_le_bytes([fmt[12], fmt[13]]);
            let w_bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);

            match format_tag {
                WAVE_FORMAT_PCM => {
                    // Use nBlockAlign to determine the actual byte width per
                    // sample, which may differ from wBitsPerSample / 8 in
                    // edge cases (e.g. 20-bit in 24-bit container).
                    if channels > 0 {
                        let container_bytes = block_align / channels;
                        bit_depth = (container_bytes * 8).min(32);
                    } else {
                        bit_depth = w_bits_per_sample;
                    }
                }
                WAVE_FORMAT_IEEE_FLOAT => {
                    // Signal to pcm_bytes_to_f32 that the data is already
                    // IEEE float.  We use 0 as a sentinel value.
                    if channels > 0 {
                        let container_bytes = block_align / channels;
                        // 32-bit float -> sentinel 0; 64-bit float -> unsupported
                        if container_bytes == 4 {
                            bit_depth = 0; // sentinel: IEEE float 32-bit
                        } else {
                            // 64-bit float — cannot handle, fall through to
                            // compressed decode path
                            return None;
                        }
                    } else {
                        return None;
                    }
                }
                WAVE_FORMAT_EXTENSIBLE => {
                    // EXTENSIBLE: wBitsPerSample is the container size.
                    // wValidBitsPerSample at fmt[18..19] is the actual depth.
                    // The sub-format GUID at fmt[24..40] tells us PCM vs Float.
                    if chunk_size >= 40 {
                        let valid_bits = u16::from_le_bytes([fmt[18], fmt[19]]);
                        // Sub-format GUID first two bytes indicate the format
                        // (same as format_tag for standard formats).
                        let sub_format = u16::from_le_bytes([fmt[24], fmt[25]]);
                        if sub_format == WAVE_FORMAT_IEEE_FLOAT {
                            if channels > 0 && block_align / channels == 4 {
                                bit_depth = 0; // sentinel: IEEE float 32-bit
                            } else {
                                return None; // 64-bit float unsupported
                            }
                        } else {
                            // PCM sub-format — use valid_bits for the actual
                            // bit depth, but the byte stride comes from
                            // nBlockAlign.
                            if channels > 0 {
                                let container_bytes = block_align / channels;
                                // Use the smaller of container size and valid
                                // bits, rounded to a standard byte width.
                                let effective = valid_bits.min(container_bytes * 8);
                                bit_depth = match effective {
                                    0..=16 => 16,
                                    17..=24 => 24,
                                    _ => 32,
                                };
                            } else {
                                bit_depth = w_bits_per_sample;
                            }
                        }
                    } else {
                        // Truncated EXTENSIBLE — fall back to container size
                        bit_depth = w_bits_per_sample;
                    }
                }
                _ => {
                    // Unknown format tag — let compressed decode handle it
                    return None;
                }
            }
        } else if chunk_id == b"data" {
            data_offset = Some(offset + 8);
            break;
        }

        offset += 8 + chunk_size;
        // Chunks are word-aligned
        if !chunk_size.is_multiple_of(2) {
            offset += 1;
        }
    }

    data_offset.map(|d| (channels, sample_rate, bit_depth, d))
}

/// Convert raw PCM bytes to f32 samples.
///
/// `bit_depth` semantics:
///   - 16: signed 16-bit little-endian integer
///   - 24: signed 24-bit little-endian integer (3 bytes per sample)
///   - 32: signed 32-bit little-endian integer
///   -  0: IEEE 754 32-bit float (already in [-1, 1] range)
fn pcm_bytes_to_f32(bytes: &[u8], bit_depth: u16) -> Vec<f32> {
    match bit_depth {
        0 => {
            // IEEE Float 32-bit — reinterpret bytes as f32 directly
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        }
        16 => bytes
            .chunks_exact(2)
            .map(|c| {
                let sample = i16::from_le_bytes([c[0], c[1]]);
                sample as f32 / 32768.0
            })
            .collect(),
        24 => bytes
            .chunks_exact(3)
            .map(|c| {
                let sample =
                    ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8; // sign-extend
                sample as f32 / 8388608.0
            })
            .collect(),
        32 => bytes
            .chunks_exact(4)
            .map(|c| {
                let sample = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                sample as f32 / 2147483648.0
            })
            .collect(),
        _ => {
            // Fall back to 16-bit
            bytes
                .chunks_exact(2)
                .map(|c| {
                    let sample = i16::from_le_bytes([c[0], c[1]]);
                    sample as f32 / 32768.0
                })
                .collect()
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for LocalOutput {
    fn name(&self) -> &str {
        &self.device_name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "local"
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        // Store the known track duration so get_status() can report it
        // and the poller can detect near-end-of-track for gapless/advance.
        if let Some(dur) = media.duration_ms {
            self.duration_ms.store(dur, Ordering::SeqCst);
        }
        self.play_url(media.url, media.mime_type, media.title, media.artist)
            .await
    }

    async fn play_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.stop().await.ok();

        // Create a FRESH force_silent flag for the new stream.
        // The old stream's callback keeps its clone of the previous Arc
        // (which was set to true by stop()), so it stays silent.
        // This prevents the race where resetting force_silent would
        // accidentally un-silence the old cpal callback.
        let new_force_silent = Arc::new(AtomicBool::new(false));
        *self.force_silent.lock().unwrap() = new_force_silent.clone();
        let force_silent = new_force_silent;

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let device_name = self.device_name.clone();
        let url = url.to_string();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let volume = self.volume.clone();
        let position_ms = self.position_ms.clone();
        let exclusive_mode = self.exclusive_mode;

        // Store metadata
        *self.current_uri.lock().unwrap() = Some(url.clone());
        *self.track_title.lock().unwrap() = title.map(String::from);
        *self.track_artist.lock().unwrap() = artist.map(String::from);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);
        // NOTE: duration_ms is NOT reset here — play_media() sets it before
        // calling play_url(), and resetting would wipe the known duration.
        // It is cleared in stop() instead.

        let handle = std::thread::spawn(move || {
            // ------- HTTP fetch the audio stream -------
            // No total timeout — long tracks can stream for 30+ minutes.
            // The force_silent flag is checked at every loop iteration and
            // in feed_ring to abort promptly on stop().
            let response = match reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .and_then(|client| client.get(&url).send())
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, url = %url, "local_audio_http_fetch_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            if !response.status().is_success() && response.status().as_u16() != 206 {
                warn!(status = %response.status(), url = %url, "local_audio_http_error");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            // Read first bytes to detect WAV header
            use std::io::Read;
            let mut reader = response;
            let mut header_buf = vec![0u8; 4096];
            let read_start = std::time::Instant::now();
            let header_read = loop {
                if force_silent.load(Ordering::Relaxed) {
                    debug!("local_audio_header_read_aborted");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                match reader.read(&mut header_buf) {
                    Ok(n) => break n,
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        // Retry header read (stream not ready yet)
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "local_audio_header_read_failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            };
            let read_elapsed = read_start.elapsed();
            debug!(
                header_bytes = header_read,
                elapsed_ms = read_elapsed.as_millis() as u64,
                "local_audio_first_read"
            );
            header_buf.truncate(header_read);

            let (channels, sample_rate, bit_depth, data_offset) = if let Some(parsed) =
                parse_wav_header(&header_buf)
            {
                info!(
                    channels = parsed.0,
                    sample_rate = parsed.1,
                    bit_depth = parsed.2,
                    data_offset = parsed.3,
                    "local_audio_wav_header_parsed"
                );
                parsed
            } else {
                // No WAV header — this is a compressed stream (FLAC, MP3, AAC).
                // Read the rest of the stream, decode with symphonia, and play.
                info!("local_audio_non_wav_stream_detected_decoding");

                // Read the entire remaining stream
                let mut all_data = header_buf.clone();
                let mut buf = vec![0u8; 65536];
                loop {
                    if stop_rx.try_recv().is_ok() {
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_compressed_read_aborted_by_stop");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => all_data.extend_from_slice(&buf[..n]),
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Read timeout — check abort flag and retry
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_compressed_read_error");
                            break;
                        }
                    }
                }

                // Decode the compressed audio
                let Some((dec_channels, dec_sample_rate, decoded_samples)) =
                    decode_compressed_stream(&all_data)
                else {
                    warn!("local_audio_decode_compressed_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                };

                // Now play the decoded f32 samples using cpal shared mode
                let dec_ch = dec_channels;
                let dec_sr = dec_sample_rate;
                let decoded_len = decoded_samples.len();

                let host = cpal::default_host();
                let device = if device_name == "default" {
                    host.default_output_device()
                } else {
                    host.output_devices().ok().and_then(|mut devs| {
                        devs.find(|d| {
                            d.description()
                                .map(|desc| {
                                    let n = desc.name().to_string();
                                    n == device_name || n.contains(&device_name)
                                })
                                .unwrap_or(false)
                        })
                    })
                };

                let Some(device) = device else {
                    warn!(name = %device_name, "audio_device_not_found");
                    playing.store(false, Ordering::SeqCst);
                    return;
                };

                // Try source sample rate first, then fall back to device default
                let output_config = find_matching_config(&device, dec_ch, dec_sr)
                    .or_else(|| device.default_output_config().ok().map(|c| c.config()))
                    .unwrap_or(cpal::StreamConfig {
                        channels: dec_ch,
                        sample_rate: dec_sr,
                        buffer_size: cpal::BufferSize::Default,
                    });

                let output_sr = output_config.sample_rate;
                let output_ch = output_config.channels;

                let ring_cap = (output_sr as usize) * (output_ch as usize) * 2;
                let ring = Arc::new(RingBuf::new(ring_cap));
                let ring_cb = ring.clone();
                let vol_cb = volume.clone();
                let paused_cb = paused.clone();
                let silent_cb = force_silent.clone();

                let stream = match device.build_output_stream(
                    &output_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return;
                        }
                        let read = ring_cb.pop(data);
                        let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
                        for sample in &mut data[..read] {
                            *sample *= v;
                        }
                        if read < data.len() {
                            data[read..].fill(0.0);
                        }
                    },
                    |e| warn!(error = %e, "audio_stream_error"),
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "audio_stream_build_failed_compressed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                info!(
                    device = %device_name,
                    dec_sr,
                    dec_ch,
                    output_sr,
                    output_ch,
                    samples = decoded_len,
                    "local_audio_compressed_playing"
                );

                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }

                // Adapt channels and resample if needed
                let mut samples = decoded_samples;
                if dec_ch != output_ch {
                    samples = adapt_channels(&samples, dec_ch, output_ch);
                }
                if dec_sr != output_sr {
                    samples = simple_resample(&samples, dec_sr, output_sr, output_ch);
                }

                // Feed all samples to ring buffer
                feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                // Update position
                let total_frames = decoded_len as u64 / dec_ch as u64;
                let duration = (total_frames as f64 / dec_sr as f64 * 1000.0) as u64;
                position_ms.store(duration, Ordering::Relaxed);

                // Wait for ring buffer to drain
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    if ring.available() == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                drop(stream);
                playing.store(false, Ordering::SeqCst);
                info!(device = %device_name, "local_audio_compressed_stopped");
                return;
            };

            // bit_depth == 0 is the sentinel for IEEE float 32-bit (4 bytes)
            let bytes_per_sample = if bit_depth == 0 {
                4
            } else {
                (bit_depth / 8) as usize
            };
            let frame_bytes = channels as usize * bytes_per_sample;

            // ------- Exclusive mode path (macOS only) -------
            #[cfg(target_os = "macos")]
            if exclusive_mode {
                use super::coreaudio_exclusive::ExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_exclusive_mode_active"
                );

                // Ring buffer: ~2 seconds of audio at source sample rate
                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                let ring = Arc::new(RingBuf::new(ring_cap));

                let exclusive = match ExclusiveOutput::new(
                    &device_name,
                    sample_rate,
                    bit_depth as u32,
                    channels as u32,
                    ring.clone(),
                    volume.clone(),
                    paused.clone(),
                ) {
                    Ok(ex) => ex,
                    Err(e) => {
                        warn!(error = %e, "coreaudio_exclusive_init_failed_falling_back_to_shared");
                        // Fall through to cpal shared mode below
                        // We need a goto-like mechanism; use a flag instead
                        // (handled by the `if !exclusive_mode` block below)
                        // Actually, we can't fall through in Rust. Log and return error.
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                info!(device = %device_name, url = %url, "local_audio_exclusive_playing");

                // Feed audio data (no resampling needed -- hardware is set to source rate)
                let pcm_data = if data_offset < header_buf.len() {
                    header_buf[data_offset..].to_vec()
                } else {
                    Vec::new()
                };

                let mut total_frames_fed: u64 = 0;

                // Read and feed the rest of the stream
                let mut read_buf = vec![0u8; 65536];
                let mut leftover: Vec<u8> = Vec::new();

                // Process leftover from header read
                if !pcm_data.is_empty() {
                    let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
                    if aligned_len > 0 {
                        let samples = pcm_bytes_to_f32(&pcm_data[..aligned_len], bit_depth);
                        feed_ring_abortable(
                            &ring,
                            &samples,
                            &stop_rx,
                            &paused,
                            Some(&force_silent),
                        );
                        total_frames_fed += (aligned_len / frame_bytes) as u64;
                    }
                    // Carry over unaligned remainder bytes
                    if aligned_len < pcm_data.len() {
                        leftover.extend_from_slice(&pcm_data[aligned_len..]);
                    }
                }

                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_exclusive_aborted_by_stop");
                        break;
                    }

                    let n = match reader.read(&mut read_buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Read timeout — check abort flag and retry
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_exclusive_read_error");
                            break;
                        }
                    };

                    leftover.extend_from_slice(&read_buf[..n]);

                    let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                    if aligned_len == 0 {
                        continue;
                    }

                    let samples = pcm_bytes_to_f32(&leftover[..aligned_len], bit_depth);
                    let remainder = leftover[aligned_len..].to_vec();
                    leftover = remainder;

                    feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                    total_frames_fed += (aligned_len / frame_bytes) as u64;

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                // Wait for ring buffer to drain
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    if ring.available() == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                // ExclusiveOutput::drop() restores sample rate and releases hog mode
                drop(exclusive);
                playing.store(false, Ordering::SeqCst);
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_exclusive_stopped"
                );
                return;
            }

            // Suppress unused-variable warning on non-macOS platforms
            #[cfg(not(target_os = "macos"))]
            let _ = exclusive_mode;

            // ------- Open cpal device (shared mode) -------
            let host = cpal::default_host();
            let device = if device_name == "default" {
                host.default_output_device()
            } else {
                host.output_devices().ok().and_then(|mut devs| {
                    devs.find(|d| {
                        d.description()
                            .map(|desc| {
                                let n = desc.name().to_string();
                                n == device_name || n.contains(&device_name)
                            })
                            .unwrap_or(false)
                    })
                })
            };

            let Some(device) = device else {
                // Log all available devices for diagnosis (e.g. USB DAC not found)
                let available: Vec<String> = host
                    .output_devices()
                    .map(|devs| {
                        devs.filter_map(|d| {
                            d.description().ok().map(|desc| desc.name().to_string())
                        })
                        .collect()
                    })
                    .unwrap_or_default();
                warn!(
                    requested = %device_name,
                    available = ?available,
                    "audio_device_not_found"
                );
                playing.store(false, Ordering::SeqCst);
                return;
            };

            // Try to find a config matching the stream's sample rate.
            // If the device doesn't explicitly list the source rate, try it
            // anyway — WASAPI shared mode will resample in the driver (better
            // quality than our linear interpolation). Only fall back to default
            // config if cpal rejects the stream config at build time.
            let preferred_config = find_matching_config(&device, channels, sample_rate)
                .unwrap_or_else(|| {
                    // Attempt source rate even if not in reported range
                    cpal::StreamConfig {
                        channels,
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    }
                });

            // Build output stream — try preferred config first, fall back to
            // device default if the device rejects the source sample rate
            // (common on Windows where WASAPI shared mode locks to 48 kHz).
            let silent_cb_outer = force_silent.clone();
            let build_stream = |cfg: &cpal::StreamConfig,
                                ring_cb: Arc<RingBuf>,
                                vol_cb: Arc<AtomicU32>,
                                paused_cb: Arc<AtomicBool>,
                                _finished_cb: Arc<AtomicBool>,
                                silent_cb: Arc<AtomicBool>| {
                device.build_output_stream(
                    cfg,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return;
                        }
                        let read = ring_cb.pop(data);
                        let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
                        for sample in &mut data[..read] {
                            *sample *= v;
                        }
                        if read < data.len() {
                            data[read..].fill(0.0);
                        }
                    },
                    |e| warn!(error = %e, "audio_stream_error"),
                    None,
                )
            };

            let finished_flag = Arc::new(AtomicBool::new(false));

            // First attempt: preferred config (source sample rate)
            let ring_cap_preferred =
                (preferred_config.sample_rate as usize) * (preferred_config.channels as usize) * 2;
            let ring_preferred = Arc::new(RingBuf::new(ring_cap_preferred));
            let stream_result = build_stream(
                &preferred_config,
                ring_preferred.clone(),
                volume.clone(),
                paused.clone(),
                finished_flag.clone(),
                silent_cb_outer.clone(),
            );

            let (stream, actual_config, ring) = match stream_result {
                Ok(s) => (s, preferred_config, ring_preferred),
                Err(first_err) => {
                    // Fall back to device default config + rubato resampling
                    let fallback_config = device.default_output_config().map(|c| c.config());
                    match fallback_config {
                        Ok(default_cfg) => {
                            info!(
                                default_sr = default_cfg.sample_rate,
                                default_ch = default_cfg.channels,
                                "local_audio_fallback_to_device_default"
                            );
                            let ring_cap_fb = (default_cfg.sample_rate as usize)
                                * (default_cfg.channels as usize)
                                * 2;
                            let ring_fb = Arc::new(RingBuf::new(ring_cap_fb));
                            match build_stream(
                                &default_cfg,
                                ring_fb.clone(),
                                volume.clone(),
                                paused.clone(),
                                finished_flag.clone(),
                                silent_cb_outer.clone(),
                            ) {
                                Ok(s) => (s, default_cfg, ring_fb),
                                Err(second_err) => {
                                    warn!(
                                        first_error = %first_err,
                                        second_error = %second_err,
                                        "audio_stream_build_failed_both_configs"
                                    );
                                    playing.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                        Err(cfg_err) => {
                            warn!(
                                first_error = %first_err,
                                config_error = %cfg_err,
                                "audio_stream_build_failed"
                            );
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            };

            let output_sr = actual_config.sample_rate;
            let output_ch = actual_config.channels;

            info!(
                device = %device_name,
                input_sr = sample_rate,
                input_bd = bit_depth,
                input_ch = channels,
                output_sr,
                output_ch,
                "local_audio_stream_config"
            );

            if let Err(e) = stream.play() {
                warn!(error = %e, "audio_stream_play_failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            info!(device = %device_name, url = %url, "local_audio_playing");

            // ------- Feed audio data from HTTP stream to ring buffer -------
            let pcm_data = if data_offset < header_buf.len() {
                header_buf[data_offset..].to_vec()
            } else {
                Vec::new()
            };

            debug!(
                pcm_data_from_header = pcm_data.len(),
                header_buf_len = header_buf.len(),
                data_offset,
                "local_audio_initial_pcm_data"
            );

            let mut total_frames_fed: u64 = 0;
            let needs_resample = output_sr != sample_rate;
            let needs_channel_adapt = output_ch != channels;

            // Create rubato sinc resampler once for the entire track.
            // Using FixedAsync::Input so we feed fixed-size input chunks.
            let mut resampler: Option<Async<f32>> = if needs_resample {
                let ratio = output_sr as f64 / sample_rate as f64;
                let sinc_len = 256;
                let window = WindowFunction::BlackmanHarris2;
                let f_cutoff = calculate_cutoff(sinc_len, window);
                let params = SincInterpolationParameters {
                    sinc_len,
                    f_cutoff,
                    interpolation: SincInterpolationType::Cubic,
                    oversampling_factor: 256,
                    window,
                };
                match Async::<f32>::new_sinc(
                    ratio,
                    1.1,
                    &params,
                    1024,
                    output_ch as usize,
                    FixedAsync::Input,
                ) {
                    Ok(r) => {
                        info!(
                            from_sr = sample_rate,
                            to_sr = output_sr,
                            "rubato_resampler_created"
                        );
                        Some(r)
                    }
                    Err(e) => {
                        warn!(error = %e, "rubato_resampler_creation_failed");
                        None
                    }
                }
            } else {
                None
            };

            // Read and feed the rest of the stream
            let mut read_buf = vec![0u8; 65536];
            // Seed the leftover buffer with any unaligned remainder from the
            // initial header read so byte alignment is preserved across reads.
            // Previously the remainder was silently dropped, causing every
            // subsequent 24-bit sample to be read from the wrong byte offset
            // (white noise).
            let mut leftover: Vec<u8> = Vec::new();

            // Process leftover from header read
            if !pcm_data.is_empty() {
                let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
                if aligned_len > 0 {
                    let mut samples = pcm_bytes_to_f32(&pcm_data[..aligned_len], bit_depth);
                    if needs_channel_adapt {
                        samples = adapt_channels(&samples, channels, output_ch);
                    }
                    if needs_resample {
                        samples = rubato_resample_chunk(&mut resampler, &samples, output_ch, false);
                    }
                    feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));
                    total_frames_fed += (aligned_len / frame_bytes) as u64;
                }
                // Carry over unaligned remainder bytes
                if aligned_len < pcm_data.len() {
                    leftover.extend_from_slice(&pcm_data[aligned_len..]);
                }
            }
            let mut total_bytes_read: u64 = 0;
            let mut first_data_logged = false;
            let stream_start = std::time::Instant::now();

            loop {
                // Check for stop signal (non-blocking)
                if stop_rx.try_recv().is_ok() {
                    debug!(
                        total_bytes_read,
                        total_frames_fed, "local_audio_stopped_by_signal"
                    );
                    break;
                }
                // Check abort flag (set by stop() to force immediate exit)
                if force_silent.load(Ordering::Relaxed) {
                    debug!(
                        total_bytes_read,
                        total_frames_fed, "local_audio_stopped_by_abort_flag"
                    );
                    break;
                }

                let read_start = std::time::Instant::now();
                let n = match reader.read(&mut read_buf) {
                    Ok(0) => {
                        debug!(
                            total_bytes_read,
                            total_frames_fed,
                            elapsed_ms = stream_start.elapsed().as_millis() as u64,
                            "local_audio_stream_eof"
                        );
                        break; // EOF
                    }
                    Ok(n) => n,
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        // Read timeout — loop back to check abort flag
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, total_bytes_read, "local_audio_read_error");
                        break;
                    }
                };
                let read_elapsed = read_start.elapsed();

                // Log first data arrival and any suspiciously slow reads
                if !first_data_logged {
                    info!(
                        bytes = n,
                        wait_ms = stream_start.elapsed().as_millis() as u64,
                        "local_audio_first_pcm_data_received"
                    );
                    first_data_logged = true;
                } else if read_elapsed.as_millis() > 5000 {
                    warn!(
                        bytes = n,
                        wait_ms = read_elapsed.as_millis() as u64,
                        total_bytes_read,
                        "local_audio_slow_read"
                    );
                }

                total_bytes_read += n as u64;
                leftover.extend_from_slice(&read_buf[..n]);

                let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                if aligned_len == 0 {
                    continue;
                }

                let mut samples = pcm_bytes_to_f32(&leftover[..aligned_len], bit_depth);
                let remainder = leftover[aligned_len..].to_vec();
                leftover = remainder;

                // Detect all-zero samples (silence from decode failure)
                if !first_data_logged || total_frames_fed == 0 {
                    let non_zero = samples.iter().any(|&s| s != 0.0);
                    if !non_zero && !samples.is_empty() {
                        warn!(
                            sample_count = samples.len(),
                            "local_audio_first_samples_all_zero"
                        );
                    }
                }

                if needs_channel_adapt {
                    samples = adapt_channels(&samples, channels, output_ch);
                }
                if needs_resample {
                    samples = rubato_resample_chunk(&mut resampler, &samples, output_ch, false);
                }

                feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                total_frames_fed += (aligned_len / frame_bytes) as u64;

                // Update position
                let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64;
                position_ms.store(pos, Ordering::Relaxed);
            }

            // Flush the resampler: feed silence to get remaining buffered samples
            if needs_resample {
                let flushed = rubato_resample_chunk(&mut resampler, &[], output_ch, true);
                if !flushed.is_empty() {
                    feed_ring_abortable(&ring, &flushed, &stop_rx, &paused, Some(&force_silent));
                }
            }

            // Signal that HTTP reading is done
            finished_flag.store(true, Ordering::SeqCst);

            // Wait for ring buffer to drain or stop signal
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                if force_silent.load(Ordering::Relaxed) {
                    break;
                }
                if ring.available() == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            drop(stream);
            playing.store(false, Ordering::SeqCst);
            info!(
                device = %device_name,
                frames = total_frames_fed,
                total_bytes_read,
                elapsed_ms = stream_start.elapsed().as_millis() as u64,
                "local_audio_stopped"
            );
        });

        *self.stop_tx.lock().unwrap() = Some(stop_tx);
        *self.play_thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        // Immediately silence the cpal callback so no audio leaks while
        // we wait for the playback thread to exit.  This flag is also
        // checked by the I/O read loop and feed_ring, causing the thread
        // to exit promptly.
        self.force_silent
            .lock()
            .unwrap()
            .store(true, Ordering::SeqCst);
        // Send the stop signal via channel (belt-and-suspenders with force_silent)
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        // Unpause so the thread unblocks from pause-wait loops
        self.paused.store(false, Ordering::SeqCst);
        // Wait for the playback thread to exit so the cpal stream is
        // dropped (releasing the audio device) before a new track starts.
        // Even if the thread is slow to exit (blocked on HTTP I/O), the
        // force_silent flag ensures silence, and play_url() creates a
        // FRESH force_silent Arc so the old callback stays permanently muted.
        let old_handle = self.play_thread.lock().unwrap().take();
        if let Some(handle) = old_handle {
            let _ = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
                loop {
                    if handle.is_finished() {
                        let _ = handle.join();
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        warn!(
                            "local_audio_stop_thread_join_timeout — old stream may overlap briefly"
                        );
                        // Thread is still running but force_silent ensures
                        // the cpal callback outputs silence, so no audible
                        // overlap. The thread will clean up on its own.
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            })
            .await;
        }
        self.playing.store(false, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
        self.duration_ms.store(0, Ordering::SeqCst);
        *self.current_uri.lock().unwrap() = None;
        *self.track_title.lock().unwrap() = None;
        *self.track_artist.lock().unwrap() = None;
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        // Seek not supported for streaming local output — the HTTP stream
        // is consumed sequentially. The orchestrator handles seek by
        // re-issuing play_url from the desired offset.
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let v = (volume.clamp(0.0, 1.0) * 1000.0) as u32;
        self.volume.store(v, Ordering::SeqCst);
        if v > 0 {
            self.muted.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            let current = self.volume.load(Ordering::SeqCst);
            if current > 0 {
                self.pre_mute_volume.store(current, Ordering::SeqCst);
            }
            self.volume.store(0, Ordering::SeqCst);
            self.muted.store(true, Ordering::SeqCst);
        } else {
            let restored = self.pre_mute_volume.load(Ordering::SeqCst);
            self.volume
                .store(if restored > 0 { restored } else { 1000 }, Ordering::SeqCst);
            self.muted.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                TransportState::Paused
            } else {
                TransportState::Playing
            }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri: self.current_uri.lock().unwrap().clone(),
            track_title: self.track_title.lock().unwrap().clone(),
            track_artist: self.track_artist.lock().unwrap().clone(),
        })
    }

    async fn is_available(&self) -> bool {
        let name = self.device_name.clone();
        // Probe on a blocking thread to avoid cpal blocking the async runtime
        tokio::task::spawn_blocking(move || {
            let host = cpal::default_host();
            if name == "default" {
                return host.default_output_device().is_some();
            }
            host.output_devices()
                .map(|devs| {
                    devs.into_iter().any(|d| {
                        d.description()
                            .map(|desc| {
                                let n = desc.name().to_string();
                                n == name || n.contains(&name)
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Feed samples into the ring buffer, blocking (with sleep) when full.
/// Checks the stop signal, abort flag, and pause state periodically.
/// Returns immediately when abort is signaled or stop is received.
fn feed_ring_abortable(
    ring: &RingBuf,
    samples: &[f32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    abort: Option<&AtomicBool>,
) {
    let mut offset = 0;
    while offset < samples.len() {
        if stop_rx.try_recv().is_ok() {
            return;
        }
        if abort.map_or(false, |a| a.load(Ordering::Relaxed)) {
            return;
        }
        // If paused, wait without feeding
        while paused.load(Ordering::Relaxed) {
            if stop_rx.try_recv().is_ok() {
                return;
            }
            if abort.map_or(false, |a| a.load(Ordering::Relaxed)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let written = ring.push(&samples[offset..]);
        offset += written;
        if written == 0 {
            // Ring buffer full — wait a bit
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Probe a device's capabilities when `supported_output_configs()` fails or
/// returns an empty set (common with PipeWire's ALSA compatibility layer).
///
/// Strategy:
/// 1. Try `default_output_config()` — this often works even when enumeration
///    doesn't (PipeWire handles it at the session-manager level).
/// 2. If that also fails, assume conservative defaults: stereo, 44100+48000 Hz.
///    PipeWire will accept these and resample internally.
fn probe_device_fallback_caps(device: &cpal::Device, name: &str) -> (u16, Vec<u32>) {
    if let Ok(default_cfg) = device.default_output_config() {
        let cfg = default_cfg.config();
        let ch = cfg.channels;
        let sr = cfg.sample_rate;
        // The default config gives us one known-good rate.  Also include
        // the other standard rate (44100 or 48000) since PipeWire's
        // resampler handles both transparently.
        let mut rates = vec![sr];
        let peer = if sr == 48000 { 44100 } else { 48000 };
        if !rates.contains(&peer) {
            rates.push(peer);
        }
        rates.sort();
        info!(
            device = %name,
            channels = ch,
            default_sr = sr,
            rates = ?rates,
            "local_audio_device_fallback_via_default_config"
        );
        (ch, rates)
    } else {
        // Last resort: assume stereo 44100/48000.  PipeWire will accept
        // these through its ALSA PCM plugin even without enumeration.
        info!(
            device = %name,
            "local_audio_device_fallback_to_assumed_stereo_44100_48000"
        );
        (2, vec![44100, 48000])
    }
}

/// Find a cpal StreamConfig that matches the desired channels and sample rate.
///
/// When `supported_output_configs()` fails (PipeWire ALSA compat), falls back
/// to `default_output_config()` and, as a last resort, returns a config with
/// the requested parameters directly — PipeWire will accept and resample.
fn find_matching_config(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
) -> Option<cpal::StreamConfig> {
    // Primary path: enumerate supported configs
    if let Ok(configs) = device.supported_output_configs() {
        let configs_vec: Vec<_> = configs.collect();
        if !configs_vec.is_empty() {
            for config in &configs_vec {
                if config.channels() >= channels
                    && config.min_sample_rate() <= sample_rate
                    && config.max_sample_rate() >= sample_rate
                {
                    return Some(cpal::StreamConfig {
                        channels: channels.min(config.channels()),
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    });
                }
            }
            // Configs exist but none match the requested rate — let caller
            // handle with its own fallback logic (e.g. try source rate anyway).
            return None;
        }
        // Empty config list — fall through to fallback
    }

    // Fallback for PipeWire / broken ALSA enumeration:
    // Try default_output_config() which often works even when enumeration fails.
    if let Ok(default_cfg) = device.default_output_config() {
        let cfg = default_cfg.config();
        // If the default config's rate matches what we want, use it directly.
        // Otherwise return the default config — the caller will resample.
        if cfg.sample_rate == sample_rate && cfg.channels >= channels {
            return Some(cpal::StreamConfig {
                channels,
                sample_rate,
                buffer_size: cpal::BufferSize::Default,
            });
        }
        // Return default config even if rate differs — better than nothing.
        // Caller will set up resampling.
        return Some(cfg);
    }

    // Last resort: return the requested config directly.  PipeWire's ALSA
    // plugin accepts arbitrary configs and resamples/remixes internally.
    // This will fail on real ALSA without PipeWire, but the caller's
    // build_output_stream error handling covers that case.
    debug!(
        channels,
        sample_rate, "find_matching_config_using_direct_params_pipewire_fallback"
    );
    Some(cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    })
}

/// Adapt channel count between source and output.
///
/// Handles upmix (mono to stereo, etc.) and downmix.  When downmixing
/// from 5.1 (6 ch) or 7.1 (8 ch) to stereo, applies ITU-R BS.775
/// compliant coefficients (K = 0.707) over the standard SMPTE/ITU
/// channel layout: FL, FR, C, LFE, SL, SR [, BL, BR].
fn adapt_channels(samples: &[f32], from_ch: u16, to_ch: u16) -> Vec<f32> {
    if from_ch == to_ch {
        return samples.to_vec();
    }
    let from = from_ch as usize;
    let to = to_ch as usize;

    let mut out = Vec::with_capacity(samples.len() * to / from);
    for frame in samples.chunks_exact(from) {
        if to > from {
            // Upmix: copy existing channels, duplicate last for remaining
            for &s in frame {
                out.push(s);
            }
            let last = *frame.last().unwrap_or(&0.0);
            for _ in from..to {
                out.push(last);
            }
        } else if to == 2 && from >= 6 {
            const K: f32 = 0.707;
            let fl = frame[0];
            let fr = frame[1];
            let c = frame[2];
            let sl = frame[4];
            let sr = frame[5];
            let (bl, br) = if from >= 8 {
                (frame[6], frame[7])
            } else {
                (0.0, 0.0)
            };
            let l = fl + K * c + K * sl + K * bl;
            let r = fr + K * c + K * sr + K * br;
            out.push(l.clamp(-1.0, 1.0));
            out.push(r.clamp(-1.0, 1.0));
        } else {
            for ch in 0..to {
                out.push(frame[ch]);
            }
        }
    }
    out
}

/// Simple linear-interpolation resampler for rate conversion.
/// Kept as a fallback — the main path now uses rubato sinc resampling.
#[allow(dead_code)]
fn simple_resample(samples: &[f32], from_sr: u32, to_sr: u32, channels: u16) -> Vec<f32> {
    if from_sr == to_sr {
        return samples.to_vec();
    }
    let ch = channels as usize;
    let in_frames = samples.len() / ch;
    if in_frames == 0 {
        return Vec::new();
    }
    let ratio = to_sr as f64 / from_sr as f64;
    let out_frames = (in_frames as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(out_frames * ch);

    for i in 0..out_frames {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;
        let idx0 = idx.min(in_frames - 1);
        let idx1 = (idx + 1).min(in_frames - 1);
        for c in 0..ch {
            let s0 = samples[idx0 * ch + c];
            let s1 = samples[idx1 * ch + c];
            out.push(s0 + (s1 - s0) * frac);
        }
    }
    out
}

/// Resample a chunk of interleaved f32 samples using rubato's sinc resampler.
///
/// The resampler is created once per track and reused across chunks.
/// `samples` is interleaved f32, `channels` is the channel count *after*
/// any channel adaptation (i.e. the output channel count).
///
/// When `flush` is true, feeds silence into the resampler to drain its
/// internal buffers at end-of-stream. `samples` should be empty in that case.
fn rubato_resample_chunk(
    resampler: &mut Option<Async<f32>>,
    samples: &[f32],
    channels: u16,
    flush: bool,
) -> Vec<f32> {
    use rubato::audioadapter_buffers::direct::InterleavedSlice;
    use rubato::audioadapter_buffers::owned::InterleavedOwned;

    let Some(resampler) = resampler.as_mut() else {
        // No resampler available — pass through unchanged
        return samples.to_vec();
    };

    let ch = channels as usize;
    let in_frames = if flush {
        // Feed enough silence to flush the resampler's internal delay
        resampler.input_frames_next()
    } else {
        if samples.is_empty() || ch == 0 {
            return Vec::new();
        }
        samples.len() / ch
    };

    if in_frames == 0 && !flush {
        return Vec::new();
    }

    // Build the input buffer — either from real samples or silence for flush
    let input_data: Vec<f32>;
    let input_ref: &[f32] = if flush {
        input_data = vec![0.0f32; in_frames * ch];
        &input_data
    } else {
        // Ensure we have a whole number of frames
        let usable = (samples.len() / ch) * ch;
        &samples[..usable]
    };
    let actual_in_frames = input_ref.len() / ch;

    if actual_in_frames == 0 {
        return Vec::new();
    }

    // Process the input in chunks of input_frames_next() size
    let mut all_output = Vec::new();
    let mut offset = 0;

    while offset < actual_in_frames || (flush && offset == 0) {
        let chunk_needed = resampler.input_frames_next();
        let chunk_available = actual_in_frames.saturating_sub(offset);

        let chunk_frames = chunk_available.min(chunk_needed);
        let is_partial = chunk_frames < chunk_needed;

        if chunk_frames == 0 && !is_partial {
            break;
        }

        let chunk_slice = &input_ref[offset * ch..(offset + chunk_frames) * ch];

        let input_adapter = match InterleavedSlice::new(chunk_slice, ch, chunk_frames) {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "rubato_input_adapter_error");
                break;
            }
        };

        let out_frames = resampler.output_frames_next();
        let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);

        let partial_len = if is_partial { Some(chunk_frames) } else { None };
        let indexing = rubato::Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len,
            active_channels_mask: None,
        };

        match resampler.process_into_buffer(&input_adapter, &mut output_buf, Some(&indexing)) {
            Ok((_nbr_in, nbr_out)) => {
                // Extract interleaved output
                let out_data = output_buf.take_data();
                all_output.extend_from_slice(&out_data[..nbr_out * ch]);
            }
            Err(e) => {
                warn!(error = %e, "rubato_process_error");
                break;
            }
        }

        offset += chunk_frames;

        // If this was a flush with a partial chunk, we're done
        if flush && is_partial {
            break;
        }
        // If we've exhausted input and it was partial, stop
        if is_partial {
            break;
        }
    }

    all_output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wav_header() {
        let header = crate::audio::wav::build_wav_header(2, 44100, 16);
        let parsed = parse_wav_header(&header);
        assert!(parsed.is_some());
        let (ch, sr, bd, offset) = parsed.unwrap();
        assert_eq!(ch, 2);
        assert_eq!(sr, 44100);
        assert_eq!(bd, 16);
        assert_eq!(offset, 44);
    }

    #[test]
    fn test_pcm_bytes_to_f32_16bit() {
        // 0x7FFF = 32767 -> ~1.0
        let bytes = [0xFF, 0x7F, 0x00, 0x00]; // 32767, 0
        let samples = pcm_bytes_to_f32(&bytes, 16);
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.99997).abs() < 0.001);
        assert!((samples[1]).abs() < 0.001);
    }

    #[test]
    fn test_pcm_bytes_to_f32_24bit() {
        let bytes = [0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x00]; // max positive, zero
        let samples = pcm_bytes_to_f32(&bytes, 24);
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 1.0).abs() < 0.001);
        assert!((samples[1]).abs() < 0.001);
    }

    #[test]
    fn test_parse_wav_header_24bit() {
        let header = crate::audio::wav::build_wav_header(2, 96000, 24);
        let parsed = parse_wav_header(&header);
        assert!(parsed.is_some());
        let (ch, sr, bd, offset) = parsed.unwrap();
        assert_eq!(ch, 2);
        assert_eq!(sr, 96000);
        assert_eq!(bd, 24);
        assert_eq!(offset, 44);
    }

    #[test]
    fn test_parse_wav_header_ieee_float() {
        // Build a 32-bit IEEE Float WAV header (format tag 3)
        let mut header = [0u8; 44];
        header[0..4].copy_from_slice(b"RIFF");
        header[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&16u32.to_le_bytes());
        header[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE_FLOAT
        header[22..24].copy_from_slice(&2u16.to_le_bytes()); // channels
        header[24..28].copy_from_slice(&44100u32.to_le_bytes());
        header[28..32].copy_from_slice(&(44100u32 * 2 * 4).to_le_bytes()); // byte_rate
        header[32..34].copy_from_slice(&8u16.to_le_bytes()); // block_align
        header[34..36].copy_from_slice(&32u16.to_le_bytes()); // bits_per_sample
        header[36..40].copy_from_slice(b"data");
        header[40..44].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

        let parsed = parse_wav_header(&header);
        assert!(parsed.is_some());
        let (ch, sr, bd, offset) = parsed.unwrap();
        assert_eq!(ch, 2);
        assert_eq!(sr, 44100);
        assert_eq!(bd, 0); // sentinel for IEEE float
        assert_eq!(offset, 44);
    }

    #[test]
    fn test_parse_wav_header_extensible_24bit() {
        // Build a WAVE_FORMAT_EXTENSIBLE 24-bit WAV header
        let mut header = [0u8; 68]; // 12 (RIFF) + 8 (fmt hdr) + 40 (fmt data) + 8 (data hdr)
        header[0..4].copy_from_slice(b"RIFF");
        header[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&40u32.to_le_bytes()); // extensible fmt size
        header[20..22].copy_from_slice(&0xFFFEu16.to_le_bytes()); // EXTENSIBLE
        header[22..24].copy_from_slice(&2u16.to_le_bytes()); // channels
        header[24..28].copy_from_slice(&96000u32.to_le_bytes());
        header[28..32].copy_from_slice(&(96000u32 * 2 * 3).to_le_bytes());
        header[32..34].copy_from_slice(&6u16.to_le_bytes()); // block_align = 2*3
        header[34..36].copy_from_slice(&24u16.to_le_bytes()); // wBitsPerSample
        header[36..38].copy_from_slice(&22u16.to_le_bytes()); // cbSize
        header[38..40].copy_from_slice(&24u16.to_le_bytes()); // wValidBitsPerSample
        header[40..44].copy_from_slice(&0u32.to_le_bytes()); // channel mask
        // Sub-format GUID: PCM = {00000001-0000-0010-8000-00aa00389b71}
        header[44..46].copy_from_slice(&1u16.to_le_bytes());
        header[46..60].copy_from_slice(&[
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ]);
        header[60..64].copy_from_slice(b"data");
        header[64..68].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

        let parsed = parse_wav_header(&header);
        assert!(parsed.is_some());
        let (ch, sr, bd, offset) = parsed.unwrap();
        assert_eq!(ch, 2);
        assert_eq!(sr, 96000);
        assert_eq!(bd, 24);
        assert_eq!(offset, 68);
    }

    #[test]
    fn test_pcm_bytes_to_f32_float() {
        // IEEE Float 32-bit: 0.5 and -0.5
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
        let samples = pcm_bytes_to_f32(&bytes, 0);
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.5).abs() < 0.0001);
        assert!((samples[1] + 0.5).abs() < 0.0001);
    }

    #[test]
    fn test_ring_buffer() {
        let ring = RingBuf::new(16);
        let data = [1.0f32, 2.0, 3.0, 4.0];
        assert_eq!(ring.push(&data), 4);
        assert_eq!(ring.available(), 4);

        let mut out = [0.0f32; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn test_ring_buffer_overflow() {
        let ring = RingBuf::new(4);
        let data = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(ring.push(&data), 4); // only 4 fit
        assert_eq!(ring.available(), 4);
    }

    #[test]
    fn test_adapt_channels_mono_to_stereo() {
        let mono = [0.5f32, 0.7];
        let stereo = adapt_channels(&mono, 1, 2);
        assert_eq!(stereo, [0.5, 0.5, 0.7, 0.7]);
    }

    #[test]
    fn test_adapt_channels_stereo_to_mono() {
        let stereo = [0.5f32, 0.7, 0.3, 0.9];
        let mono = adapt_channels(&stereo, 2, 1);
        assert_eq!(mono, [0.5, 0.3]);
    }

    #[test]
    fn test_simple_resample_same_rate() {
        let data = [1.0f32, 2.0, 3.0, 4.0];
        let out = simple_resample(&data, 44100, 44100, 2);
        assert_eq!(out, data);
    }

    #[test]
    fn test_simple_resample_upsample() {
        let data = [0.0f32, 0.0, 1.0, 1.0]; // 2 frames stereo
        let out = simple_resample(&data, 44100, 88200, 2);
        // Should produce ~4 frames
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn test_pcm_bytes_to_f32_24bit_negative() {
        // 24-bit minimum: 0x800000 = -8388608 -> -1.0
        let bytes = [0x00, 0x00, 0x80]; // -8388608
        let samples = pcm_bytes_to_f32(&bytes, 24);
        assert_eq!(samples.len(), 1);
        assert!(
            (samples[0] + 1.0).abs() < 0.001,
            "expected -1.0, got {}",
            samples[0]
        );

        // Small negative: 0xFFFFFF = -1 -> ~ -0.000000119
        let bytes2 = [0xFF, 0xFF, 0xFF];
        let samples2 = pcm_bytes_to_f32(&bytes2, 24);
        assert_eq!(samples2.len(), 1);
        assert!(samples2[0] < 0.0, "expected negative, got {}", samples2[0]);
    }

    #[test]
    fn test_24bit_frame_alignment() {
        // Simulate the scenario that caused white noise: initial read
        // from a WAV stream where the PCM data after the header is NOT
        // a multiple of frame_bytes (6 for 24-bit stereo).
        //
        // Build a WAV header + 8 bytes of PCM (6 aligned + 2 remainder).
        let wav_hdr = crate::audio::wav::build_wav_header(2, 44100, 24);
        assert_eq!(wav_hdr.len(), 44);

        // 2 channels * 3 bytes = 6 bytes per frame
        let frame_bytes: usize = 6;

        // Create 8 bytes of PCM data (1 full frame + 2 leftover bytes)
        let pcm_data: Vec<u8> = vec![
            // Frame 0: L=0x000001 R=0x000002
            0x01, 0x00, 0x00, 0x02, 0x00, 0x00, // Frame 1 partial: first 2 bytes
            0x03, 0x00,
        ];

        // Simulate the old buggy code: only process aligned, drop remainder
        let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
        assert_eq!(aligned_len, 6);
        let remainder = pcm_data.len() - aligned_len;
        assert_eq!(remainder, 2, "there should be 2 leftover bytes");

        // The fix: carry remainder into leftover buffer
        let mut leftover: Vec<u8> = Vec::new();
        if aligned_len < pcm_data.len() {
            leftover.extend_from_slice(&pcm_data[aligned_len..]);
        }
        assert_eq!(leftover.len(), 2);
        assert_eq!(leftover, vec![0x03, 0x00]);

        // Simulate next read arriving: 4 more bytes complete frame 1
        let next_read: Vec<u8> = vec![0x00, 0x04, 0x00, 0x00];
        leftover.extend_from_slice(&next_read);
        // Now leftover has 6 bytes = 1 complete frame
        let aligned_len2 = (leftover.len() / frame_bytes) * frame_bytes;
        assert_eq!(aligned_len2, 6);
        let samples = pcm_bytes_to_f32(&leftover[..aligned_len2], 24);
        assert_eq!(samples.len(), 2); // L and R of frame 1
    }

    #[test]
    fn test_list_audio_devices() {
        // Should not panic, even if no devices available
        let devices = list_audio_devices();
        // On CI there may be no devices, but on dev machines there should be at least one
        let _ = devices.len();
    }
}
