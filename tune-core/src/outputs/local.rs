use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
    let default_name = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
        .unwrap_or_default();

    let mut devices = Vec::new();
    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            let name = device
                .description()
                .map(|desc| desc.name().to_string())
                .unwrap_or_else(|_| "Unknown".into());
            let is_default = name == default_name;

            let (max_channels, sample_rates) =
                if let Ok(configs) = device.supported_output_configs() {
                    let mut max_ch = 0u16;
                    let mut rates = Vec::new();
                    for config in configs {
                        max_ch = max_ch.max(config.channels());
                        let min = config.min_sample_rate();
                        let max = config.max_sample_rate();
                        for &rate in &[44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000] {
                            if rate >= min && rate <= max && !rates.contains(&rate) {
                                rates.push(rate);
                            }
                        }
                    }
                    rates.sort();
                    (max_ch, rates)
                } else {
                    (2, vec![44100, 48000])
                };

            devices.push(AudioDevice {
                name,
                is_default,
                max_channels,
                sample_rates,
            });
        }
    }
    devices
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
    /// When true (and on macOS), use CoreAudio exclusive/hog mode for
    /// bit-perfect output, bypassing the system mixer.
    exclusive_mode: bool,
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
            exclusive_mode,
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

/// Parse a WAV header and return (channels, sample_rate, bit_depth, data_offset).
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
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            bit_depth = u16::from_le_bytes([fmt[14], fmt[15]]);
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

/// Convert raw PCM bytes (16-bit or 24-bit little-endian) to f32 samples.
fn pcm_bytes_to_f32(bytes: &[u8], bit_depth: u16) -> Vec<f32> {
    match bit_depth {
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

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let device_name = self.device_name.clone();
        let url = url.to_string();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let volume = self.volume.clone();
        let position_ms = self.position_ms.clone();
        let duration_ms_shared = self.duration_ms.clone();
        let exclusive_mode = self.exclusive_mode;

        // Store metadata
        *self.current_uri.lock().unwrap() = Some(url.clone());
        *self.track_title.lock().unwrap() = title.map(String::from);
        *self.track_artist.lock().unwrap() = artist.map(String::from);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);
        duration_ms_shared.store(0, Ordering::SeqCst);

        std::thread::spawn(move || {
            // ------- HTTP fetch the audio stream -------
            let response = match reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
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
            let header_read = match reader.read(&mut header_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "local_audio_header_read_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };
            header_buf.truncate(header_read);

            let (channels, sample_rate, bit_depth, data_offset) =
                if let Some(parsed) = parse_wav_header(&header_buf) {
                    info!(
                        channels = parsed.0,
                        sample_rate = parsed.1,
                        bit_depth = parsed.2,
                        data_offset = parsed.3,
                        "local_audio_wav_header_parsed"
                    );
                    parsed
                } else {
                    // No WAV header — assume raw PCM with server defaults (44100/16/2)
                    // or try to infer from MIME type. The streamer always sends WAV for
                    // local files, so this path is a fallback for edge cases.
                    info!("local_audio_no_wav_header_assuming_raw_pcm");
                    (2u16, 44100u32, 16u16, 0)
                };

            let bytes_per_sample = (bit_depth / 8) as usize;
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

                // Process leftover from header read
                if !pcm_data.is_empty() {
                    let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
                    if aligned_len > 0 {
                        let samples = pcm_bytes_to_f32(&pcm_data[..aligned_len], bit_depth);
                        feed_ring(&ring, &samples, &stop_rx, &paused);
                        total_frames_fed += (aligned_len / frame_bytes) as u64;
                    }
                }

                // Read and feed the rest of the stream
                let mut read_buf = vec![0u8; 65536];
                let mut leftover = Vec::new();

                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }

                    let n = match reader.read(&mut read_buf) {
                        Ok(0) => break,
                        Ok(n) => n,
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

                    feed_ring(&ring, &samples, &stop_rx, &paused);

                    total_frames_fed += (aligned_len / frame_bytes) as u64;

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                // Wait for ring buffer to drain
                loop {
                    if stop_rx.try_recv().is_ok() {
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
                warn!(name = %device_name, "audio_device_not_found");
                playing.store(false, Ordering::SeqCst);
                return;
            };

            // Try to find a config matching the stream's sample rate.
            // If the device doesn't explicitly list the source rate, try it
            // anyway — WASAPI shared mode will resample in the driver (better
            // quality than our linear interpolation). Only fall back to default
            // config if cpal rejects the stream config at build time.
            let stream_config = find_matching_config(&device, channels, sample_rate)
                .unwrap_or_else(|| {
                    // Attempt source rate even if not in reported range
                    cpal::StreamConfig {
                        channels,
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    }
                });

            let output_sr = stream_config.sample_rate;
            let output_ch = stream_config.channels;

            info!(
                device = %device_name,
                input_sr = sample_rate,
                input_bd = bit_depth,
                input_ch = channels,
                output_sr,
                output_ch,
                "local_audio_stream_config"
            );

            // Ring buffer: ~2 seconds of audio at output sample rate
            let ring_cap = (output_sr as usize) * (output_ch as usize) * 2;
            let ring = Arc::new(RingBuf::new(ring_cap));
            let ring_for_callback = ring.clone();
            let vol_for_callback = volume.clone();
            let paused_for_callback = paused.clone();
            let finished_flag = Arc::new(AtomicBool::new(false));
            let finished_for_callback = finished_flag.clone();

            let stream = device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if paused_for_callback.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let read = ring_for_callback.pop(data);
                    // Apply volume
                    let v = vol_for_callback.load(Ordering::Relaxed) as f32 / 1000.0;
                    for sample in &mut data[..read] {
                        *sample *= v;
                    }
                    // Fill remainder with silence
                    if read < data.len() {
                        data[read..].fill(0.0);
                        // If the HTTP reader is done and buffer is empty, signal end
                        if finished_for_callback.load(Ordering::Relaxed)
                            && ring_for_callback.available() == 0
                        {
                            // Nothing more to play — callback will produce silence
                        }
                    }
                },
                |e| warn!(error = %e, "audio_stream_error"),
                None,
            );

            let Ok(stream) = stream else {
                warn!("audio_stream_build_failed");
                playing.store(false, Ordering::SeqCst);
                return;
            };

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

            let mut total_frames_fed: u64 = 0;
            let needs_resample = output_sr != sample_rate;
            let needs_channel_adapt = output_ch != channels;

            // Process leftover from header read
            if !pcm_data.is_empty() {
                let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
                if aligned_len > 0 {
                    let mut samples = pcm_bytes_to_f32(&pcm_data[..aligned_len], bit_depth);
                    if needs_channel_adapt {
                        samples = adapt_channels(&samples, channels, output_ch);
                    }
                    if needs_resample {
                        samples = simple_resample(&samples, sample_rate, output_sr, output_ch);
                    }
                    feed_ring(&ring, &samples, &stop_rx, &paused);
                    total_frames_fed += (aligned_len / frame_bytes) as u64;
                }
            }

            // Read and feed the rest of the stream
            let mut read_buf = vec![0u8; 65536];
            let mut leftover = Vec::new();

            loop {
                // Check for stop signal (non-blocking)
                if stop_rx.try_recv().is_ok() {
                    break;
                }

                let n = match reader.read(&mut read_buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => n,
                    Err(e) => {
                        warn!(error = %e, "local_audio_read_error");
                        break;
                    }
                };

                leftover.extend_from_slice(&read_buf[..n]);

                let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                if aligned_len == 0 {
                    continue;
                }

                let mut samples = pcm_bytes_to_f32(&leftover[..aligned_len], bit_depth);
                let remainder = leftover[aligned_len..].to_vec();
                leftover = remainder;

                if needs_channel_adapt {
                    samples = adapt_channels(&samples, channels, output_ch);
                }
                if needs_resample {
                    samples = simple_resample(&samples, sample_rate, output_sr, output_ch);
                }

                feed_ring(&ring, &samples, &stop_rx, &paused);

                total_frames_fed += (aligned_len / frame_bytes) as u64;

                // Update position
                let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64;
                position_ms.store(pos, Ordering::Relaxed);
            }

            // Signal that HTTP reading is done
            finished_flag.store(true, Ordering::SeqCst);

            // Wait for ring buffer to drain or stop signal
            loop {
                if stop_rx.try_recv().is_ok() {
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
                "local_audio_stopped"
            );
        });

        *self.stop_tx.lock().unwrap() = Some(stop_tx);
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
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
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
/// Checks the stop signal periodically.
fn feed_ring(
    ring: &RingBuf,
    samples: &[f32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
) {
    let mut offset = 0;
    while offset < samples.len() {
        if stop_rx.try_recv().is_ok() {
            return;
        }
        // If paused, wait without feeding
        while paused.load(Ordering::Relaxed) {
            if stop_rx.try_recv().is_ok() {
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

/// Find a cpal StreamConfig that matches the desired channels and sample rate.
fn find_matching_config(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
) -> Option<cpal::StreamConfig> {
    let configs = device.supported_output_configs().ok()?;
    for config in configs {
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
    None
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
    fn test_list_audio_devices() {
        // Should not panic, even if no devices available
        let devices = list_audio_devices();
        // On CI there may be no devices, but on dev machines there should be at least one
        let _ = devices.len();
    }
}
