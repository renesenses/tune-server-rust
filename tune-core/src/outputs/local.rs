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
use crate::poller::TRACK_END_NOTIFY;

// ---------------------------------------------------------------------------
// Audio host selection (WASAPI vs ASIO on Windows)
// ---------------------------------------------------------------------------

/// Select the cpal host based on the requested backend.
///
/// - `"asio"`: use the ASIO host (requires `asio` cargo feature; Windows only)
/// - `"wasapi"`: use the default host (WASAPI on Windows)
/// - `"auto"` (default): try ASIO first if available, fall back to default
///
/// On non-Windows platforms, always returns `cpal::default_host()`.
pub fn select_host(backend: &str) -> cpal::Host {
    let backend_lower = backend.to_lowercase();

    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        #[cfg(all(target_os = "windows", feature = "asio"))]
        super::asio_exclusive::ensure_com_initialized();
        match backend_lower.as_str() {
            "asio" => match cpal::host_from_id(cpal::HostId::Asio) {
                Ok(host) => {
                    let device_count = host.output_devices().map(|d| d.count()).unwrap_or(0);
                    if device_count > 0 {
                        info!(
                            backend = "asio",
                            devices = device_count,
                            "local_audio_host_selected"
                        );
                        return host;
                    }
                    warn!(
                        "local_audio_asio_no_devices — ASIO host OK but no output devices found, falling back to WASAPI"
                    );
                    return cpal::default_host();
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "local_audio_asio_host_unavailable — check ASIO driver installation"
                    );
                    info!(backend = "wasapi", "local_audio_host_fallback");
                    return cpal::default_host();
                }
            },
            "auto" => {
                // Auto mode uses WASAPI directly — ASIO drivers can call
                // abort() when probed, crashing the process silently.
                // Users who want ASIO must set TUNE_AUDIO_BACKEND=asio.
                info!(backend = "wasapi", "local_audio_host_selected_auto");
                return cpal::default_host();
            }
            _ => {
                info!(backend = "wasapi", "local_audio_host_selected");
                return cpal::default_host();
            }
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        let _ = &backend_lower;
        if backend_lower == "asio" {
            warn!(
                "local_audio_asio_requested_but_not_available — \
                 ASIO requires Windows and the `asio` cargo feature"
            );
        }
        cpal::default_host()
    }
}

/// Returns the name of the audio backend for the given preference.
pub fn active_backend_name(backend: &str) -> &'static str {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        match backend.to_lowercase().as_str() {
            "asio" => "ASIO",
            "wasapi" => "WASAPI",
            "auto" => "WASAPI",
            _ => "WASAPI",
        }
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        let _ = backend;
        #[cfg(target_os = "windows")]
        {
            "WASAPI"
        }
        #[cfg(target_os = "macos")]
        {
            "CoreAudio"
        }
        #[cfg(target_os = "linux")]
        {
            "ALSA"
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            "default"
        }
    }
}

/// Returns `true` if this build includes ASIO support.
pub fn asio_available() -> bool {
    cfg!(all(target_os = "windows", feature = "asio"))
}

/// List ASIO audio output devices specifically.
///
/// On Windows with the `asio` feature enabled, this enumerates devices using
/// the ASIO host (bypassing WASAPI).  On other platforms or without the `asio`
/// feature, returns an empty list.
///
/// Each returned `AsioDeviceInfo` includes the driver name, supported sample
/// rates, max channels, and whether it's the default ASIO device.
pub fn list_asio_devices() -> Vec<AsioDeviceInfo> {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        use std::sync::Mutex as StdMutex;

        // Last successful enumeration. Served verbatim while an exclusive stream
        // owns the ASIO device, so listing never re-opens a driver that is
        // already locked for playback.
        static ASIO_DEVICE_CACHE: StdMutex<Option<Vec<AsioDeviceInfo>>> = StdMutex::new(None);

        let enumerate = || -> Vec<AsioDeviceInfo> {
            super::asio_exclusive::ensure_com_initialized();
            let host = match cpal::host_from_id(cpal::HostId::Asio) {
                Ok(h) => h,
                Err(e) => {
                    warn!(error = %e, "asio_device_enumeration_failed — no ASIO host available");
                    return Vec::new();
                }
            };

            let default_name = host
                .default_output_device()
                .and_then(|d| d.description().ok())
                .map(|desc| desc.name().to_string())
                .unwrap_or_default();

            let mut devices = Vec::new();
            match host.output_devices() {
                Ok(output_devices) => {
                    for device in output_devices {
                        let name = device
                            .description()
                            .map(|desc| desc.name().to_string())
                            .unwrap_or_else(|_| "Unknown ASIO Device".into());

                        let is_default = name == default_name;

                        let (max_channels, sample_rates) = match device.supported_output_configs() {
                            Ok(configs) => {
                                let mut max_ch = 0u16;
                                let mut rates = Vec::new();
                                for config in configs {
                                    max_ch = max_ch.max(config.channels());
                                    let min = config.min_sample_rate();
                                    let max = config.max_sample_rate();
                                    for &rate in &[
                                        44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000,
                                        705600, 768000,
                                    ] {
                                        if rate >= min && rate <= max && !rates.contains(&rate) {
                                            rates.push(rate);
                                        }
                                    }
                                }
                                rates.sort();
                                (max_ch, rates)
                            }
                            Err(_) => {
                                // ASIO drivers usually enumerate correctly, but fall
                                // back to conservative defaults if they don't.
                                (2, vec![44100, 48000, 96000, 192000])
                            }
                        };

                        info!(
                            name = %name,
                            is_default,
                            max_channels,
                            sample_rates = ?sample_rates,
                            "asio_device_found"
                        );

                        devices.push(AsioDeviceInfo {
                            name,
                            is_default,
                            max_channels,
                            sample_rates,
                            exclusive: true, // ASIO is always exclusive
                        });
                    }
                }
                Err(e) => {
                    warn!(error = %e, "asio_output_devices_enumeration_failed");
                }
            }

            devices
        };

        // Probe the driver ONLY when no exclusive stream currently owns it.
        // Re-opening the single-instance ASIO driver while a zone is playing
        // churns it — on SOtM Diretta it never finishes locking (endless
        // connect → getBufferSize → disconnect cycles, never reaching
        // createBuffers/start). When the device is busy, serve the cache.
        match super::asio_exclusive::try_with_asio_device_lock(enumerate) {
            Some(devices) => {
                *ASIO_DEVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(devices.clone());
                devices
            }
            None => {
                let cached = ASIO_DEVICE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_default();
                debug!(
                    cached_devices = cached.len(),
                    "asio_device_enumeration_skipped_playback_active"
                );
                cached
            }
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        Vec::new()
    }
}

/// Information about an ASIO audio device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsioDeviceInfo {
    /// ASIO driver name (e.g. "RME Babyface Pro FS ASIO").
    pub name: String,
    /// Whether this is the default ASIO output device.
    pub is_default: bool,
    /// Maximum number of output channels supported.
    pub max_channels: u16,
    /// Supported sample rates (Hz).
    pub sample_rates: Vec<u32>,
    /// ASIO devices are always in exclusive mode.
    pub exclusive: bool,
}

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub is_default: bool,
    pub max_channels: u16,
    pub sample_rates: Vec<u32>,
    /// The audio backend this device was enumerated from.
    #[serde(default)]
    pub backend: String,
}

static SCAN_GUARD: std::sync::Mutex<Option<(std::time::Instant, Vec<AudioDevice>)>> =
    std::sync::Mutex::new(None);
const SCAN_COOLDOWN_SECS: u64 = 5;

/// List audio devices using the default host.
pub fn list_audio_devices() -> Vec<AudioDevice> {
    list_audio_devices_with_backend("auto")
}

/// List audio devices using the specified backend preference.
/// Protected by a global Mutex + 5s cache to prevent concurrent ASIO
/// driver enumeration which crashes on Windows (non-reentrant COM STA).
pub fn list_audio_devices_with_backend(backend: &str) -> Vec<AudioDevice> {
    let mut guard = SCAN_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((last_scan, ref cached)) = *guard {
        if last_scan.elapsed().as_secs() < SCAN_COOLDOWN_SECS {
            debug!("local_audio_scan_cached");
            return cached.clone();
        }
    }
    let result = list_audio_devices_uncached(backend);
    *guard = Some((std::time::Instant::now(), result.clone()));
    result
}

/// Return the last cached device list WITHOUT triggering a fresh enumeration.
///
/// Enumerating WASAPI devices probes each device's supported formats, which can
/// invalidate an active render stream and kill playback on Windows (DEvir). So
/// while a local stream is playing we serve this cache instead of re-scanning.
/// Returns an empty list if nothing has been enumerated yet this session.
pub fn cached_audio_devices() -> Vec<AudioDevice> {
    SCAN_GUARD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, devices)| devices.clone())
        .unwrap_or_default()
}

fn list_audio_devices_uncached(backend: &str) -> Vec<AudioDevice> {
    let host = select_host(backend);
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

    let mut devices: Vec<AudioDevice> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    // Signature = (raw name, caps). Windows WASAPI can list the same physical
    // endpoint (onboard "HDA ..." codecs) more than once with an identical name
    // AND identical capabilities; those true duplicates are collapsed so they
    // don't spawn a phantom second zone (Elie).
    // On Linux, PipeWire re-exposes the SAME physical output many times with
    // *different* reported capabilities (e.g. "ALC255 Analog" as 2ch/48k, then
    // 32ch/384k, then a stereo fallback), so the (name, caps) signature above
    // never collapses them and each variant becomes a phantom zone (JeromeQ:
    // 43 devices → 48 zones on Ubuntu 24.04). Collapse by NAME instead, keeping
    // the richest-capability variant. Maps raw device name → index into `devices`.
    #[cfg(target_os = "linux")]
    let mut linux_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    match host.output_devices() {
        Ok(output_devices) => {
            for device in output_devices {
                let raw_name = device
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "Unknown".into());

                // Skip ALSA null/dummy sinks that produce no audio
                if raw_name.contains("Discard all samples") || raw_name.contains("Dummy") {
                    debug!(device = %raw_name, "local_audio_device_skipped_null_sink");
                    continue;
                }

                let (max_channels, sample_rates, caps_reliable) =
                    match device.supported_output_configs() {
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
                                    device = %raw_name,
                                    "local_audio_device_supported_configs_empty"
                                );
                                probe_device_fallback_caps(&device, &raw_name)
                            } else {
                                // Enumerated caps are real → safe to collapse on.
                                (max_ch, rates, true)
                            }
                        }
                        Err(_) => {
                            debug!(
                                device = %raw_name,
                                "local_audio_device_supported_configs_failed"
                            );
                            probe_device_fallback_caps(&device, &raw_name)
                        }
                    };

                let is_default = raw_name == default_name;
                // caps_reliable was only read by the removed (name, caps) collapse
                // (Linux collapses by name; Windows/macOS now keep every device).
                let _ = caps_reliable;

                // Collapse duplicates. On Linux PipeWire lists the same physical
                // output repeatedly with varying caps, so collapse by NAME and
                // keep the richest-capability variant (else 43 phantom devices →
                // 48 zones, JeromeQ on Ubuntu 24.04). On Windows/macOS two real
                // DACs can share a name but differ in caps, so collapse only exact
                // (name, caps) duplicates and disambiguate the rest.
                #[cfg(target_os = "linux")]
                {
                    if let Some(&idx) = linux_by_name.get(&raw_name) {
                        let richer = max_channels > devices[idx].max_channels
                            || (max_channels == devices[idx].max_channels
                                && sample_rates.len() > devices[idx].sample_rates.len());
                        if richer {
                            devices[idx].max_channels = max_channels;
                            devices[idx].sample_rates = sample_rates.clone();
                        }
                        if is_default {
                            devices[idx].is_default = true;
                        }
                        debug!(device = %raw_name, "local_audio_device_collapsed_pipewire_duplicate");
                        continue;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    // Windows/macOS: do NOT collapse — always disambiguate below.
                    // Two genuinely different physical devices can share BOTH the
                    // name AND the caps: Alain's Ugreen card and his USB DAC both
                    // enumerate as "Speakers" with identical reliable caps, so the
                    // old (name, caps) collapse dropped the DAC entirely and it
                    // could never get a zone (#1084) — even after #654, because
                    // its caps are real, not the assumed fallback. cpal exposes no
                    // unique WASAPI endpoint id to tell a true duplicate from two
                    // same-named devices, so keep every entry and disambiguate
                    // ("Speakers (2)"), restoring the pre-0.8.314 behaviour Alain
                    // had on 0.8.307. A rare truly-duplicated onboard endpoint
                    // then merely shows twice (harmless — both select the same
                    // output) instead of a real device silently vanishing.
                }

                // Disambiguate duplicate device names (common on Windows WASAPI
                // where multiple USB DACs all show as "Haut-Parleurs").
                let name = if seen_names.contains(&raw_name) {
                    let mut n = 2;
                    loop {
                        let candidate = format!("{raw_name} ({n})");
                        if !seen_names.contains(&candidate) {
                            break candidate;
                        }
                        n += 1;
                    }
                } else {
                    raw_name.clone()
                };
                seen_names.insert(name.clone());

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
                    backend: host_name.to_string(),
                });
                #[cfg(target_os = "linux")]
                linux_by_name.insert(raw_name.clone(), devices.len() - 1);
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
// Gapless: pending next track for seamless chaining
// ---------------------------------------------------------------------------

/// Stores the next track's metadata for gapless playback.
/// When the current track reaches clean HTTP EOF and this is set,
/// the playback thread chains directly into the next track without
/// closing/reopening the audio device.
#[derive(Clone)]
struct PendingNextMedia {
    url: String,
    title: Option<String>,
    artist: Option<String>,
    duration_ms: Option<u64>,
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
    /// Offset added to position_ms when stream was seeked (the decoded stream
    /// starts at byte 0 but represents audio from seek_offset_ms onward).
    seek_offset_ms: Arc<AtomicU64>,
    /// One-shot start position supplied by play_media() for recreated seek
    /// streams. play_url() consumes this after stop() clears the old state.
    pending_start_position_ms: AtomicU64,
    /// When true, the audio consumer should NOT skip bytes based on
    /// seek_offset_ms because the decoder already produced a seeked stream.
    /// seek_offset_ms is still used for position reporting (progress bar).
    stream_pre_seeked: AtomicBool,
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
    /// Audio backend preference: "auto", "wasapi", or "asio" (Windows only).
    audio_backend: String,
    /// Set by stop() to immediately silence the cpal callback, even if
    /// the playback thread hasn't exited yet.  Prevents overlapping audio
    /// when switching tracks and the old thread is still draining.
    ///
    /// IMPORTANT: This is replaced with a fresh Arc on each new play_url()
    /// call, so that resetting it to `false` for the new stream does NOT
    /// accidentally un-silence the old stream's callback (which keeps its
    /// own clone of the previous Arc).
    force_silent: std::sync::Mutex<Arc<AtomicBool>>,
    play_generation: Arc<AtomicU64>,
    /// Set by the playback thread when it reaches end-of-stream naturally
    /// (i.e. the HTTP source was fully consumed, not stopped by stop()).
    ///
    /// When true, `get_status()` reports the track as still Playing but
    /// with position_ms past the track end, so the poller's
    /// `position_past_end` path fires and triggers auto_next — bypassing
    /// the gapless-guard window that would otherwise delay (or swallow)
    /// the track-end signal when the thread is detached before draining.
    ///
    /// Cleared on every `play_url()` and `stop()` call.
    track_ended_naturally: Arc<AtomicBool>,
    /// The play-generation that set `track_ended_naturally = true`.
    ///
    /// When the playback thread signals natural end-of-stream, it also
    /// stores its own `my_generation` here.  `get_status()` only honours
    /// the flag when the generation matches the *current*
    /// `play_generation`, preventing a detached old thread from
    /// contaminating the new track's status.
    track_ended_generation: Arc<AtomicU64>,
    /// Pending next track for gapless playback.  Set by `set_next_media()`,
    /// consumed by the playback thread when the current track reaches EOF.
    next_media: Arc<std::sync::Mutex<Option<PendingNextMedia>>>,
    convolver: Arc<std::sync::Mutex<Option<super::super::audio::convolver::Convolver>>>,
}

impl LocalOutput {
    pub fn new(device_name: String) -> Self {
        Self::with_options(device_name, false, "auto")
    }

    /// Create a new `LocalOutput` with explicit exclusive-mode control.
    pub fn new_with_exclusive(device_name: String, exclusive_mode: bool) -> Self {
        Self::with_options(device_name, exclusive_mode, "auto")
    }

    /// Create a new `LocalOutput` with full control over exclusive mode and
    /// audio backend selection.
    pub fn with_options(device_name: String, exclusive_mode: bool, audio_backend: &str) -> Self {
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
            seek_offset_ms: Arc::new(AtomicU64::new(0)),
            pending_start_position_ms: AtomicU64::new(0),
            stream_pre_seeked: AtomicBool::new(false),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(std::sync::Mutex::new(None)),
            track_title: Arc::new(std::sync::Mutex::new(None)),
            track_artist: Arc::new(std::sync::Mutex::new(None)),
            stop_tx: std::sync::Mutex::new(None),
            play_thread: std::sync::Mutex::new(None),
            exclusive_mode,
            audio_backend: audio_backend.to_string(),
            play_generation: Arc::new(AtomicU64::new(0)),
            force_silent: std::sync::Mutex::new(Arc::new(AtomicBool::new(false))),
            track_ended_naturally: Arc::new(AtomicBool::new(false)),
            track_ended_generation: Arc::new(AtomicU64::new(0)),
            next_media: Arc::new(std::sync::Mutex::new(None)),
            convolver: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn set_convolver_ir(&self, path: &str) -> Result<(), String> {
        let conv = super::super::audio::convolver::Convolver::from_wav(path, 1024)?;
        *self.convolver.lock().unwrap() = Some(conv);
        tracing::info!(path, device = %self.device_name, "convolver_ir_set");
        Ok(())
    }

    pub fn clear_convolver(&self) {
        *self.convolver.lock().unwrap() = None;
        tracing::info!(device = %self.device_name, "convolver_cleared");
    }

    pub fn has_convolver(&self) -> bool {
        self.convolver.lock().unwrap().is_some()
    }

    /// Returns `true` if exclusive/bit-perfect mode is supported on this platform.
    pub fn supports_exclusive_mode() -> bool {
        cfg!(target_os = "macos") || cfg!(all(target_os = "windows", feature = "asio"))
    }

    pub fn set_pending_start_position_ms(&self, position_ms: u64) {
        self.pending_start_position_ms
            .store(position_ms, Ordering::SeqCst);
    }

    /// Signal that the producer actually emitted a pre-seeked stream.
    /// Only call this when the decoder used seek_s (local files).
    /// Do NOT call for streaming sources (TIDAL/Qobuz) where the
    /// producer always starts from 0s.
    pub fn set_producer_seeked(&self, seeked: bool) {
        self.stream_pre_seeked.store(seeked, Ordering::SeqCst);
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

    /// Reset the ring buffer: zero out the underlying storage and reset
    /// the read/write cursors.  Called on track change to ensure no stale
    /// PCM data from a previous track leaks into the new one.
    pub fn clear(&self) {
        // Reset cursors first so the reader sees an empty buffer
        self.read.store(0, Ordering::SeqCst);
        self.write.store(0, Ordering::SeqCst);
        // Zero-fill the underlying storage to eliminate stale samples.
        // Safety: single-threaded clear (called before the cpal callback
        // starts reading from a freshly created ring buffer).
        let cap = self.buf.len();
        unsafe {
            let ptr = self.buf.as_ptr() as *mut f32;
            for i in 0..cap {
                *ptr.add(i) = 0.0;
            }
        }
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
/// Whether a failed header read should be retried rather than treated as a hard
/// failure. When a gapless/next track's transcode session has just started, its
/// WAV header isn't emitted yet, so the first reads return `TimedOut`/
/// `WouldBlock`. The pre-#522 code `break`-ed on any error, abandoning the chain
/// and skipping track 2 in a gapless album (Alain #981). Retrying on these
/// transient kinds — while a real error (broken pipe, etc.) still fails fast —
/// is what aligns the gapless path with the direct `play_url` path.
fn header_read_should_retry(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

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

    /// Exclusive-mode playback (ASIO / WASAPI exclusive) uses a dedicated loop
    /// that returns at EOF without consuming the staged `next_media`, so it
    /// cannot chain internally — the poller must fall back to natural-end
    /// advance. Only the shared cpal path performs internal gapless chaining.
    fn supports_internal_gapless(&self) -> bool {
        !self.exclusive_mode
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        *self.next_media.lock().unwrap() = Some(PendingNextMedia {
            url: url.to_string(),
            title: title.map(String::from),
            artist: artist.map(String::from),
            duration_ms: None,
        });
        debug!("local_audio_gapless_next_url_set");
        Ok(())
    }

    async fn set_next_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        *self.next_media.lock().unwrap() = Some(PendingNextMedia {
            url: media.url.to_string(),
            title: media.title.map(String::from),
            artist: media.artist.map(String::from),
            duration_ms: media.duration_ms,
        });
        info!(
            title = ?media.title,
            "local_audio_gapless_next_media_set"
        );
        Ok(())
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        let result = self
            .play_url(media.url, media.mime_type, media.title, media.artist)
            .await;
        // Store duration AFTER play_url() because play_url() calls stop()
        // which resets duration_ms to 0.
        if let Some(dur) = media.duration_ms {
            self.duration_ms.store(dur, Ordering::SeqCst);
        }
        result
    }

    async fn play_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.stop().await.ok();

        // Restore seek position after stop() cleared the old state.
        let start_position_ms = self.pending_start_position_ms.swap(0, Ordering::SeqCst);
        self.seek_offset_ms
            .store(start_position_ms, Ordering::SeqCst);
        self.position_ms.store(start_position_ms, Ordering::SeqCst);
        // stream_pre_seeked is set explicitly by set_producer_seeked()
        // from the orchestrator — only when the decoder actually applied
        // the seek (local files). For streaming sources (TIDAL/Qobuz),
        // the producer starts from 0s and needs consumer-side skip.

        // Clear any staged gapless next — starting from scratch.
        *self.next_media.lock().unwrap() = None;

        // Brief pause after stopping the old stream to allow the OS audio
        // subsystem (CoreAudio / WASAPI / ALSA) to fully release the device.
        // Without this, reopening the device immediately can cause the first
        // few hundred milliseconds of the new stream to contain stale data
        // from the previous session, perceived as white noise / static.
        //
        // On Windows, ASIO/WASAPI needs time to fully release the device.
        // ASIO exclusive is slower to release (~500ms for driver teardown).
        #[cfg(target_os = "windows")]
        {
            let delay = if self.audio_backend == "asio" {
                500
            } else {
                200
            };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        #[cfg(not(target_os = "windows"))]
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Create a FRESH force_silent flag for the new stream.
        // The old stream's callback keeps its clone of the previous Arc
        // (which was set to true by stop()), so it stays silent.
        // This prevents the race where resetting force_silent would
        // accidentally un-silence the old cpal callback.
        let new_force_silent = Arc::new(AtomicBool::new(false));
        *self.force_silent.lock().unwrap() = new_force_silent.clone();
        let force_silent = new_force_silent;

        let my_generation = self.play_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let play_generation = self.play_generation.clone();

        // Clear the natural-end flag and generation for the new track.
        self.track_ended_naturally.store(false, Ordering::SeqCst);
        self.track_ended_generation.store(0, Ordering::SeqCst);
        let track_ended_naturally = self.track_ended_naturally.clone();
        let track_ended_generation = self.track_ended_generation.clone();

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let device_name = self.device_name.clone();
        let url = url.to_string();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let volume = self.volume.clone();
        let position_ms = self.position_ms.clone();
        let mut seek_offset = self.seek_offset_ms.load(Ordering::SeqCst);
        let seek_offset_arc = self.seek_offset_ms.clone();
        let pre_seeked = self.stream_pre_seeked.load(Ordering::SeqCst);
        let duration_ms_arc = self.duration_ms.clone();
        let exclusive_mode = self.exclusive_mode;
        let audio_backend = self.audio_backend.clone();
        let convolver = self.convolver.clone();
        // Arcs for gapless metadata updates from the playback thread
        let next_media_ref = self.next_media.clone();
        let uri_ref = self.current_uri.clone();
        let title_ref = self.track_title.clone();
        let artist_ref = self.track_artist.clone();

        // Store metadata
        *self.current_uri.lock().unwrap() = Some(url.clone());
        *self.track_title.lock().unwrap() = title.map(String::from);
        *self.track_artist.lock().unwrap() = artist.map(String::from);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(seek_offset, Ordering::SeqCst);
        // NOTE: duration_ms is NOT reset here — play_media() sets it before
        // calling play_url(), and resetting would wipe the known duration.
        // It is cleared in stop() instead.

        let handle = std::thread::spawn(move || {
            // ------- HTTP fetch the audio stream -------
            // No total timeout — long tracks can stream for 30+ minutes.
            // The force_silent flag is checked at every loop iteration and
            // in feed_ring to abort promptly on stop().
            let response = match crate::http::client::blocking_builder()
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
                    Err(ref e) if header_read_should_retry(e.kind()) => {
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

            let (mut channels, mut sample_rate, mut bit_depth, data_offset) = if let Some(parsed) =
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

                let host = select_host(&audio_backend);
                let Some((device, fell_back)) = find_device_with_fallback(&host, &device_name)
                else {
                    warn!(name = %device_name, "audio_device_not_found_compressed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                };
                if fell_back {
                    info!(
                        original = %device_name,
                        "audio_device_fallback_used_for_compressed_stream"
                    );
                }

                // Prefer device's default rate and resample if needed.
                // Same rationale as the WAV path: opening at the source
                // rate in shared mode is unreliable on macOS/Windows.
                let output_config = {
                    let default_cfg = device.default_output_config().ok().map(|c| c.config());
                    let default_sr = default_cfg.as_ref().map(|c| c.sample_rate);
                    if default_sr == Some(dec_sr) {
                        default_cfg.unwrap()
                    } else if let Some(cfg) = default_cfg {
                        info!(
                            source_sr = dec_sr,
                            device_sr = cfg.sample_rate,
                            "local_audio_compressed_rate_mismatch_will_resample"
                        );
                        cfg
                    } else {
                        find_matching_config(&device, dec_ch, dec_sr).unwrap_or(
                            cpal::StreamConfig {
                                channels: dec_ch,
                                sample_rate: dec_sr,
                                buffer_size: cpal::BufferSize::Default,
                            },
                        )
                    }
                };

                let output_sr = output_config.sample_rate;
                let output_ch = output_config.channels;

                let ring_cap = (output_sr as usize) * (output_ch as usize) * 2;
                let ring = Arc::new(RingBuf::new(ring_cap));
                ring.clear(); // Defensive: zero-fill before callback can read
                let ring_cb = ring.clone();
                let vol_cb = volume.clone();
                let paused_cb = paused.clone();
                let silent_cb = force_silent.clone();
                // Gate: output silence until enough real data has been buffered.
                // Prevents stale/garbage audio during track transitions.
                // Minimum: ~500ms of audio at the output sample rate.
                // (v0.8.97=20ms, v0.8.98=200ms — still too low for macOS
                // CoreAudio which can request 1024+ frame buffers.)
                let data_started = Arc::new(AtomicBool::new(false));
                let data_started_cb = data_started.clone();
                let min_buffer_samples = (output_sr as usize) * (output_ch as usize) / 2; // ~500ms

                let stream = match device.build_output_stream(
                    &output_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return;
                        }
                        // Wait for a minimum amount of data before starting
                        // to read from the ring buffer. This prevents the
                        // audio device from playing stale/garbage samples
                        // during track transitions.
                        if !data_started_cb.load(Ordering::Acquire) {
                            if ring_cb.available() < min_buffer_samples {
                                data.fill(0.0);
                                return;
                            }
                            data_started_cb.store(true, Ordering::Release);
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

                // Adapt channels and resample if needed (using rubato
                // sinc resampler for high-quality rate conversion)
                let mut samples = decoded_samples;
                if dec_ch != output_ch {
                    samples = adapt_channels(&samples, dec_ch, output_ch);
                }
                if dec_sr != output_sr {
                    samples = rubato_resample_batch(&samples, dec_sr, output_sr, output_ch);
                }

                // Pre-fill the ring buffer before starting the cpal stream.
                // For compressed streams all data is already decoded, so we
                // push as much as fits (~200ms or more) before calling play().
                let prefill_target = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms
                let prefill_count = samples.len().min(prefill_target.max(ring.capacity() / 2));
                let initial_written = ring.push(&samples[..prefill_count]);

                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                info!(
                    device = %device_name,
                    prefill_samples = initial_written,
                    "local_audio_compressed_playing_after_prefill"
                );

                // Feed remaining samples to ring buffer, updating position
                // progressively so the seek bar advances during playback.
                let total_output_samples = samples.len() as u64;
                let output_frames = total_output_samples / output_ch as u64;
                let output_duration_ms = (output_frames as f64 / output_sr as f64 * 1000.0) as u64;
                let mut fed_samples = initial_written as u64;

                if initial_written < samples.len() {
                    let chunk_size = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms chunks
                    let remaining = &samples[initial_written..];
                    for chunk in remaining.chunks(chunk_size) {
                        if stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed) {
                            break;
                        }
                        while paused.load(Ordering::Relaxed)
                            && !force_silent.load(Ordering::Relaxed)
                        {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        feed_ring_abortable(&ring, chunk, &stop_rx, &paused, Some(&force_silent));
                        fed_samples += chunk.len() as u64;
                        let fed_frames = fed_samples / output_ch as u64;
                        let pos =
                            (fed_frames as f64 / output_sr as f64 * 1000.0) as u64 + seek_offset;
                        position_ms
                            .store(pos.min(output_duration_ms + seek_offset), Ordering::Relaxed);
                    }
                }

                position_ms.store(output_duration_ms + seek_offset, Ordering::Relaxed);

                // Signal natural track end BEFORE draining so the
                // orchestrator can detect end-of-track even if a new play
                // command sets force_silent while the ring buffer is still
                // being consumed (e.g. resampling 44.1→192 kHz).
                // play_url() clears this flag for the next track.
                track_ended_naturally.store(true, Ordering::SeqCst);
                track_ended_generation.store(my_generation, Ordering::SeqCst);
                TRACK_END_NOTIFY.notify_one();

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
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(device = %device_name, "local_audio_compressed_stopped");
                return;
            };

            // bit_depth == 0 is the sentinel for IEEE float 32-bit (4 bytes)
            let bytes_per_sample = if bit_depth == 0 {
                4
            } else {
                (bit_depth / 8) as usize
            };
            let mut frame_bytes = channels as usize * bytes_per_sample;

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
                ring.clear(); // Defensive: zero-fill before callback reads

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

                let mut http_eof_excl = false;
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_exclusive_aborted_by_stop");
                        break;
                    }

                    let n = match reader.read(&mut read_buf) {
                        Ok(0) => {
                            http_eof_excl = true;
                            break;
                        }
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
                            http_eof_excl = true;
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

                    if let Ok(mut conv) = convolver.lock() {
                        if let Some(ref mut c) = *conv {
                            c.process_interleaved(&mut samples);
                        }
                    }

                    feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                    total_frames_fed += (aligned_len / frame_bytes) as u64;

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64
                        + seek_offset;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                // Signal natural track end BEFORE draining when the HTTP
                // stream reached EOF, so the orchestrator can detect
                // end-of-track even if force_silent is set during slow drain.
                if http_eof_excl {
                    track_ended_naturally.store(true, Ordering::SeqCst);
                    track_ended_generation.store(my_generation, Ordering::SeqCst);
                    TRACK_END_NOTIFY.notify_one();
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
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_exclusive_stopped"
                );
                return;
            }

            // ------- Exclusive mode path (Windows ASIO) -------
            #[cfg(all(target_os = "windows", feature = "asio"))]
            if exclusive_mode && audio_backend == "asio" {
                use super::asio_exclusive::AsioExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_asio_exclusive_mode_active"
                );

                // Ring buffer: ~2 seconds of audio at source sample rate
                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                let ring = Arc::new(RingBuf::new(ring_cap));
                ring.clear(); // Defensive: zero-fill before callback reads

                let exclusive = match AsioExclusiveOutput::new(
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
                        warn!(error = %e, "asio_exclusive_init_failed_falling_back_to_shared");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                info!(device = %device_name, url = %url, "local_audio_asio_exclusive_playing");

                // Feed audio data (no resampling needed -- hardware is set to source rate)
                let pcm_data = if data_offset < header_buf.len() {
                    header_buf[data_offset..].to_vec()
                } else {
                    Vec::new()
                };

                let mut total_frames_fed: u64 = 0;

                // Only skip bytes if the stream was NOT pre-seeked by the
                // decoder. When pre_seeked=true, the decoder already produced
                // audio starting at the seek position — skipping would discard
                // the entire stream (double-seek bug reported by DEvir).
                let skip_bytes_asio: u64 = if seek_offset > 0 && !pre_seeked {
                    let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
                    skip_frames * channels as u64 * bytes_per_sample as u64
                } else {
                    0
                };
                let mut skipped_bytes_asio: u64 = 0;

                // Read and feed the rest of the stream
                let mut read_buf = vec![0u8; 65536];
                let mut leftover: Vec<u8> = Vec::new();

                // Process leftover from header read
                if !pcm_data.is_empty() {
                    if skip_bytes_asio > 0 && skipped_bytes_asio < skip_bytes_asio {
                        let remaining = (skip_bytes_asio - skipped_bytes_asio) as usize;
                        if pcm_data.len() <= remaining {
                            skipped_bytes_asio += pcm_data.len() as u64;
                        } else {
                            skipped_bytes_asio = skip_bytes_asio;
                            let kept = &pcm_data[remaining..];
                            let aligned_len = (kept.len() / frame_bytes) * frame_bytes;
                            if aligned_len > 0 {
                                let samples = pcm_bytes_to_f32(&kept[..aligned_len], bit_depth);
                                feed_ring_abortable(
                                    &ring,
                                    &samples,
                                    &stop_rx,
                                    &paused,
                                    Some(&force_silent),
                                );
                                total_frames_fed += (aligned_len / frame_bytes) as u64;
                            }
                            if aligned_len < kept.len() {
                                leftover.extend_from_slice(&kept[aligned_len..]);
                            }
                        }
                    } else {
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
                        if aligned_len < pcm_data.len() {
                            leftover.extend_from_slice(&pcm_data[aligned_len..]);
                        }
                    }
                }

                let mut http_eof_asio = false;
                let mut last_data_at = std::time::Instant::now();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_asio_exclusive_aborted_by_stop");
                        break;
                    }

                    let n = match reader.read(&mut read_buf) {
                        Ok(0) => {
                            http_eof_asio = true;
                            break;
                        }
                        Ok(n) => {
                            last_data_at = std::time::Instant::now();
                            n
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // A streaming HTTP source (transcoded WAV over a
                            // keep-alive connection) may never return a clean
                            // EOF: after the last byte it just keeps timing out.
                            // Once the whole track has been fed AND the ring has
                            // fully drained (everything played), a sustained read
                            // idle means the track ended — signal EOF so the
                            // orchestrator can advance/repeat. Without this, the
                            // loop spins forever and end-of-track is never
                            // detected on exclusive ASIO outputs (DEvir: repeat
                            // never fired on a clean playthrough).
                            if total_frames_fed > 0
                                && leftover.is_empty()
                                && ring.available() == 0
                                && last_data_at.elapsed() > std::time::Duration::from_secs(5)
                            {
                                info!("local_audio_asio_exclusive_stream_idle_eof");
                                http_eof_asio = true;
                                break;
                            }
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_asio_exclusive_read_error");
                            http_eof_asio = true;
                            break;
                        }
                    };

                    if skip_bytes_asio > 0 && skipped_bytes_asio < skip_bytes_asio {
                        let remaining = (skip_bytes_asio - skipped_bytes_asio) as usize;
                        if n <= remaining {
                            skipped_bytes_asio += n as u64;
                            continue;
                        }
                        skipped_bytes_asio = skip_bytes_asio;
                        leftover.extend_from_slice(&read_buf[remaining..n]);
                    } else {
                        leftover.extend_from_slice(&read_buf[..n]);
                    }

                    let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                    if aligned_len == 0 {
                        continue;
                    }

                    let mut samples = pcm_bytes_to_f32(&leftover[..aligned_len], bit_depth);
                    let remainder = leftover[aligned_len..].to_vec();
                    leftover = remainder;

                    if let Ok(mut conv) = convolver.lock() {
                        if let Some(ref mut c) = *conv {
                            c.process_interleaved(&mut samples);
                        }
                    }

                    feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                    total_frames_fed += (aligned_len / frame_bytes) as u64;

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64
                        + seek_offset;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                // Signal natural track end BEFORE draining when the HTTP
                // stream reached EOF, so the orchestrator can detect
                // end-of-track even if force_silent is set during slow drain.
                if http_eof_asio {
                    track_ended_naturally.store(true, Ordering::SeqCst);
                    track_ended_generation.store(my_generation, Ordering::SeqCst);
                    TRACK_END_NOTIFY.notify_one();
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

                // AsioExclusiveOutput::drop() releases the ASIO device
                drop(exclusive);
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_asio_exclusive_stopped"
                );
                return;
            }

            // ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------
            #[cfg(target_os = "windows")]
            if exclusive_mode && audio_backend != "asio" {
                use super::wasapi_exclusive::WasapiExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_wasapi_exclusive_mode_active"
                );

                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                let ring = Arc::new(RingBuf::new(ring_cap));
                ring.clear();

                match WasapiExclusiveOutput::new(
                    &device_name,
                    sample_rate,
                    bit_depth as u32,
                    channels as u32,
                    ring.clone(),
                    volume.clone(),
                    paused.clone(),
                ) {
                    Ok(mut wasapi) => {
                        if let Err(e) = wasapi.start() {
                            warn!(error = %e, "wasapi_exclusive_start_failed_falling_back");
                        } else {
                            info!(
                                device = %device_name,
                                info = %wasapi.format_info(),
                                "wasapi_exclusive_playing"
                            );

                            let pcm_data = if data_offset < header_buf.len() {
                                header_buf[data_offset..].to_vec()
                            } else {
                                Vec::new()
                            };

                            let mut total_frames_fed: u64 = 0;
                            let mut read_buf = vec![0u8; 65536];
                            let mut leftover: Vec<u8> = Vec::new();

                            if !pcm_data.is_empty() {
                                let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
                                if aligned_len > 0 {
                                    let samples =
                                        pcm_bytes_to_f32(&pcm_data[..aligned_len], bit_depth);
                                    feed_ring_abortable(
                                        &ring,
                                        &samples,
                                        &stop_rx,
                                        &paused,
                                        Some(&force_silent),
                                    );
                                    total_frames_fed += (aligned_len / frame_bytes) as u64;
                                }
                                if aligned_len < pcm_data.len() {
                                    leftover.extend_from_slice(&pcm_data[aligned_len..]);
                                }
                            }

                            let mut http_eof_wasapi = false;
                            loop {
                                if stop_rx.try_recv().is_ok() {
                                    break;
                                }
                                if force_silent.load(Ordering::Relaxed) {
                                    debug!("local_audio_wasapi_exclusive_aborted_by_stop");
                                    break;
                                }

                                match reader.read(&mut read_buf) {
                                    Ok(0) => {
                                        http_eof_wasapi = true;
                                        break;
                                    }
                                    Ok(n) => {
                                        leftover.extend_from_slice(&read_buf[..n]);
                                        let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                                        if aligned > 0 {
                                            let samples =
                                                pcm_bytes_to_f32(&leftover[..aligned], bit_depth);
                                            feed_ring_abortable(
                                                &ring,
                                                &samples,
                                                &stop_rx,
                                                &paused,
                                                Some(&force_silent),
                                            );
                                            total_frames_fed += (aligned / frame_bytes) as u64;
                                            leftover.drain(..aligned);
                                        }

                                        let pos = (total_frames_fed as f64 / sample_rate as f64
                                            * 1000.0)
                                            as u64
                                            + seek_offset;
                                        position_ms.store(pos, Ordering::Relaxed);
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::TimedOut
                                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                                    {
                                        continue;
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "local_audio_wasapi_exclusive_read_error");
                                        http_eof_wasapi = true;
                                        break;
                                    }
                                }
                            }

                            // Signal natural track end BEFORE draining when
                            // the HTTP stream reached EOF, so the orchestrator
                            // can detect end-of-track even if force_silent is
                            // set during slow drain (e.g. 44.1→192 kHz resample).
                            if http_eof_wasapi {
                                track_ended_naturally.store(true, Ordering::SeqCst);
                                track_ended_generation.store(my_generation, Ordering::SeqCst);
                                TRACK_END_NOTIFY.notify_one();
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

                            wasapi.stop();
                            if play_generation.load(Ordering::SeqCst) == my_generation {
                                playing.store(false, Ordering::SeqCst);
                            }
                            info!(
                                device = %device_name,
                                frames = total_frames_fed,
                                "local_audio_wasapi_exclusive_stopped"
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "wasapi_exclusive_init_failed_falling_back_to_shared"
                        );
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", all(target_os = "windows", feature = "asio"))))]
            let _ = exclusive_mode;

            // ------- Open cpal device (shared mode) -------
            let host = select_host(&audio_backend);
            let Some((device, fell_back)) = find_device_with_fallback(&host, &device_name) else {
                warn!(
                    requested = %device_name,
                    "audio_device_not_found_no_fallback"
                );
                playing.store(false, Ordering::SeqCst);
                return;
            };
            if fell_back {
                info!(
                    original = %device_name,
                    "audio_device_fallback_used_for_wav_stream"
                );
            }

            // Determine the output config for shared mode.
            //
            // Strategy: prefer the device's default/native sample rate and
            // resample with rubato when the source rate differs.  This is more
            // reliable than trying to open the device at the source rate:
            //
            // - On macOS, cpal's CoreAudio backend does NOT call
            //   `set_sample_rate` for output streams (only for input).  So
            //   `build_output_stream` at 96 kHz "succeeds" (CoreAudio inserts
            //   an internal converter), but the conversion is unreliable on
            //   many devices/macOS versions and produces white noise.
            //
            // - On Windows WASAPI shared mode, the system mixer runs at a
            //   fixed rate (usually 48 kHz); requesting a different rate may
            //   be rejected or silently mis-converted.
            //
            // By always opening at the device's native rate and doing our own
            // high-quality sinc resampling (rubato), we guarantee correct
            // output on all platforms.
            //
            // If the source rate happens to match the device rate, no
            // resampling occurs (zero overhead).
            let output_config = {
                // First, get the device's default config (reflects actual
                // operating rate on most platforms).
                let default_cfg = device.default_output_config().ok().map(|c| c.config());
                let default_sr = default_cfg.as_ref().map(|c| c.sample_rate);

                if default_sr == Some(sample_rate) {
                    // Device is already at the source rate — use it directly
                    default_cfg.unwrap()
                } else if let Some(cfg) = find_matching_config(&device, channels, sample_rate)
                    .filter(|c| c.sample_rate == sample_rate)
                {
                    // Device SUPPORTS the source rate even though its current
                    // default differs — open at the source rate for bit-perfect
                    // output and to avoid an extreme realtime resample.  A DSD256
                    // file decodes to 352.8kHz; on a DAC left at 44.1kHz by the
                    // OS the old code resampled 352.8k→44.1k in real time, the
                    // sinc resampler underran and no sound came out (Cyrille,
                    // FiiO K3 which natively supports 352.8kHz, iFi Neo iDSD).
                    info!(
                        source_sr = sample_rate,
                        device_default_sr = ?default_sr,
                        "local_audio_open_at_source_rate_supported"
                    );
                    // macOS: cpal's CoreAudio backend does NOT switch the device's
                    // hardware nominal rate for output streams (see the note
                    // above), so opening the cpal stream "at the source rate"
                    // leaves the DAC clocked at the OS rate and CoreAudio silently
                    // converts — which yields SILENCE for high-rate DSD→PCM
                    // (DSD128/256/512 all decode to 352.8kHz; only DSD64's 176.4k
                    // survived). We reach this branch precisely when the device
                    // SUPPORTS the source rate but its default differs, so set the
                    // hardware nominal rate explicitly (what the exclusive/hog path
                    // already does) — the DAC then actually clocks at 352.8kHz.
                    // Best-effort: if the device can't be resolved/set we fall
                    // through to today's behavior (no regression). Cyrille: iFi
                    // Neo iDSD / FiiO K3, DSD128+ silent.
                    #[cfg(target_os = "macos")]
                    {
                        use coreaudio::audio_unit::macos_helpers;
                        if let Some(dev_id) =
                            macos_helpers::get_device_id_from_name(&device_name, false)
                        {
                            let want = cfg.sample_rate as f64;
                            match macos_helpers::set_device_sample_rate(dev_id, want) {
                                Ok(_) => info!(
                                    device = %device_name,
                                    to = cfg.sample_rate,
                                    "local_audio_coreaudio_nominal_rate_set_shared"
                                ),
                                Err(e) => warn!(
                                    error = %e,
                                    wanted = cfg.sample_rate,
                                    "local_audio_coreaudio_set_rate_failed"
                                ),
                            }
                        }
                    }
                    cfg
                } else if let Some(cfg) = default_cfg {
                    // Device does not support the source rate — open at device
                    // rate, rubato will resample.
                    info!(
                        source_sr = sample_rate,
                        device_sr = cfg.sample_rate,
                        "local_audio_rate_mismatch_will_resample"
                    );
                    cfg
                } else {
                    // No default config available — try source rate as last
                    // resort (PipeWire, etc.).
                    find_matching_config(&device, channels, sample_rate).unwrap_or(
                        cpal::StreamConfig {
                            channels,
                            sample_rate,
                            buffer_size: cpal::BufferSize::Default,
                        },
                    )
                }
            };

            // Build output stream at the chosen rate.
            let silent_cb_outer = force_silent.clone();
            // Gate: the cpal callback outputs silence until enough real data
            // has been buffered in the ring buffer.  This prevents stale or
            // garbage audio from reaching the DAC during track transitions.
            let data_started_shared = Arc::new(AtomicBool::new(false));
            let build_stream = |cfg: &cpal::StreamConfig,
                                ring_cb: Arc<RingBuf>,
                                vol_cb: Arc<AtomicU32>,
                                paused_cb: Arc<AtomicBool>,
                                _finished_cb: Arc<AtomicBool>,
                                silent_cb: Arc<AtomicBool>,
                                ds_cb: Arc<AtomicBool>,
                                min_buf: usize| {
                device.build_output_stream(
                    cfg,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return;
                        }
                        // Wait for a minimum amount of data before starting
                        // to read from the ring buffer. This prevents the
                        // audio device from playing stale/garbage samples
                        // during track transitions.
                        if !ds_cb.load(Ordering::Acquire) {
                            if ring_cb.available() < min_buf {
                                data.fill(0.0);
                                return;
                            }
                            ds_cb.store(true, Ordering::Release);
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

            let ring_cap =
                (output_config.sample_rate as usize) * (output_config.channels as usize) * 2;
            let ring_buf = Arc::new(RingBuf::new(ring_cap));
            ring_buf.clear(); // Defensive: zero-fill before callback can read
            // Minimum buffer: ~200ms of audio before the callback starts reading.
            // sr * ch / 5 = 200ms of interleaved samples.
            let min_buffer =
                (output_config.sample_rate as usize) * (output_config.channels as usize) / 5;
            let stream_result = build_stream(
                &output_config,
                ring_buf.clone(),
                volume.clone(),
                paused.clone(),
                finished_flag.clone(),
                silent_cb_outer.clone(),
                data_started_shared.clone(),
                min_buffer,
            );

            let (stream, actual_config, ring) = match stream_result {
                Ok(s) => (s, output_config, ring_buf),
                Err(first_err) => {
                    // Last resort: try the source sample rate directly —
                    // some platforms (PipeWire) accept arbitrary rates.
                    let source_cfg = cpal::StreamConfig {
                        channels,
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let ring_cap_fb =
                        (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) * 2;
                    let ring_fb = Arc::new(RingBuf::new(ring_cap_fb));
                    ring_fb.clear();
                    data_started_shared.store(false, Ordering::SeqCst);
                    let min_buffer_fb =
                        (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) / 2;
                    match build_stream(
                        &source_cfg,
                        ring_fb.clone(),
                        volume.clone(),
                        paused.clone(),
                        finished_flag.clone(),
                        silent_cb_outer.clone(),
                        data_started_shared.clone(),
                        min_buffer_fb,
                    ) {
                        Ok(s) => {
                            info!(
                                source_sr = sample_rate,
                                "local_audio_fallback_to_source_rate"
                            );
                            (s, source_cfg, ring_fb)
                        }
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

            // DO NOT call stream.play() yet — we pre-fill the ring buffer
            // first to prevent CoreAudio from pulling uninitialized/empty
            // buffers in the first few callbacks.  The stream is started
            // after enough data has been buffered (~200ms).

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
            let skip_bytes: u64 = if seek_offset > 0 && !pre_seeked {
                let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
                skip_frames * channels as u64 * bytes_per_sample as u64
            } else {
                0
            };
            let mut skipped_bytes: u64 = 0;
            let mut needs_resample = output_sr != sample_rate;
            let mut needs_channel_adapt = output_ch != channels;

            // Create rubato sinc resampler once for the entire track.
            // Using FixedAsync::Input so we feed fixed-size input chunks.
            let mut resampler: Option<Async<f32>> = if needs_resample {
                let ratio = output_sr as f64 / sample_rate as f64;
                // Adaptive resampler params based on conversion ratio:
                //   ratio ≤ 2.0 (e.g. 96kHz→48kHz): quality params, plenty of CPU budget
                //   ratio > 2.0 (e.g. 176.4kHz→48kHz, 192kHz→48kHz): lighter params
                //     to avoid real-time stuttering on Windows (still ~90dB SNR)
                let inv_ratio = 1.0 / ratio; // > 1.0 when downsampling
                let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
                    (32_usize, 64_usize) // lighter: 176.4/192kHz → 48kHz
                } else {
                    (64_usize, 128_usize) // standard: 96kHz → 48kHz
                };
                let window = WindowFunction::BlackmanHarris2;
                let f_cutoff = calculate_cutoff(sinc_len, window);
                let params = SincInterpolationParameters {
                    sinc_len,
                    f_cutoff,
                    interpolation: SincInterpolationType::Linear,
                    oversampling_factor,
                    window,
                };
                info!(
                    from_sr = sample_rate,
                    to_sr = output_sr,
                    sinc_len,
                    oversampling_factor,
                    "rubato_resampler_adaptive_params"
                );
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
            // Buffer for resampler frame leftover: holds samples that don't
            // fill a complete resampler block, carried over to the next read.
            let mut resample_leftover: Vec<f32> = Vec::new();

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

                    // Diagnostic: log first few f32 samples and detect anomalies.
                    // White noise manifests as high-amplitude random values in
                    // what should be a gentle attack.
                    if !samples.is_empty() {
                        let first_8: Vec<f32> = samples.iter().take(8).copied().collect();
                        let max_abs = samples
                            .iter()
                            .take(200)
                            .fold(0.0f32, |m, &s| m.max(s.abs()));
                        let non_zero = samples.iter().take(200).filter(|&&s| s != 0.0).count();
                        info!(
                            first_samples = ?first_8,
                            max_abs_200 = max_abs,
                            non_zero_in_200 = non_zero,
                            total_samples = samples.len(),
                            bit_depth,
                            frame_bytes,
                            "local_audio_initial_samples_diagnostic"
                        );
                    }

                    if needs_channel_adapt {
                        samples = adapt_channels(&samples, channels, output_ch);
                    }
                    if needs_resample {
                        samples = rubato_resample_chunk(
                            &mut resampler,
                            &samples,
                            output_ch,
                            false,
                            &mut resample_leftover,
                        );
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

            // Pre-fill the ring buffer before starting the cpal stream.
            // Target: ~500ms of audio so the first callback has enough data.
            let prefill_target = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms
            let mut stream_started = false;

            // Check if initial header data was enough to meet the prefill target
            if ring.available() >= prefill_target {
                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                stream_started = true;
                info!(
                    device = %device_name,
                    prefill_samples = ring.available(),
                    "local_audio_playing_after_prefill"
                );
            }

            // Tracks whether the HTTP read loop exited because the source
            // reached EOF (true) vs. a stop signal or read error (false).
            // Only when http_eof=true do we signal track_ended_naturally.
            let mut http_eof = false;

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
                        http_eof = true;
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
                        http_eof = true;
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

                // Seek skip: discard PCM bytes until we reach the seek offset
                if skip_bytes > 0 && skipped_bytes < skip_bytes {
                    let remaining_to_skip = (skip_bytes - skipped_bytes) as usize;
                    if n <= remaining_to_skip {
                        skipped_bytes += n as u64;
                        continue;
                    }
                    // Partial skip: some bytes to discard, rest to process
                    let start = remaining_to_skip;
                    skipped_bytes = skip_bytes;
                    leftover.extend_from_slice(&read_buf[start..n]);
                } else {
                    leftover.extend_from_slice(&read_buf[..n]);
                }

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

                if let Ok(mut conv) = convolver.lock() {
                    if let Some(ref mut c) = *conv {
                        c.process_interleaved(&mut samples);
                    }
                }

                if needs_channel_adapt {
                    samples = adapt_channels(&samples, channels, output_ch);
                }
                if needs_resample {
                    samples = rubato_resample_chunk(
                        &mut resampler,
                        &samples,
                        output_ch,
                        false,
                        &mut resample_leftover,
                    );
                }

                feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));

                total_frames_fed += (aligned_len / frame_bytes) as u64;

                // Start the cpal stream once enough data has been pre-filled.
                // This ensures the audio device never pulls from an empty/sparse
                // ring buffer, eliminating white noise at track start.
                if !stream_started && ring.available() >= prefill_target {
                    if let Err(e) = stream.play() {
                        warn!(error = %e, "audio_stream_play_failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    stream_started = true;
                    info!(
                        device = %device_name,
                        prefill_samples = ring.available(),
                        total_bytes_read,
                        elapsed_ms = stream_start.elapsed().as_millis() as u64,
                        "local_audio_playing_after_prefill"
                    );
                }

                // Update position
                let pos =
                    (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
                position_ms.store(pos, Ordering::Relaxed);
            }

            // If the stream was never started (very short track or error),
            // start it now with whatever data we have.
            if !stream_started {
                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed_final");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                info!(
                    device = %device_name,
                    ring_available = ring.available(),
                    "local_audio_playing_short_track_or_eof"
                );
            }

            // ---------------------------------------------------------------
            // Gapless continuation: when the current track reached clean EOF
            // and a next track was staged via set_next_media(), seamlessly
            // chain into the next track without closing the cpal stream.
            // The audio device stays open — zero gap between tracks.
            // ---------------------------------------------------------------
            while http_eof && !force_silent.load(Ordering::Relaxed) {
                let pending = next_media_ref.lock().unwrap().take();
                let Some(next) = pending else { break };

                track_ended_naturally.store(false, Ordering::SeqCst);
                track_ended_generation.store(0, Ordering::SeqCst);

                info!(
                    next_title = ?next.title,
                    next_url = %next.url,
                    "local_audio_gapless_chaining_next_track"
                );

                // Flush the current resampler before switching tracks
                if needs_resample {
                    let flushed = rubato_resample_chunk(
                        &mut resampler,
                        &[],
                        output_ch,
                        true,
                        &mut resample_leftover,
                    );
                    if !flushed.is_empty() {
                        feed_ring_abortable(
                            &ring,
                            &flushed,
                            &stop_rx,
                            &paused,
                            Some(&force_silent),
                        );
                    }
                }

                // Update shared metadata for the new track so get_status()
                // and the poller see the transition.
                *uri_ref.lock().unwrap() = Some(next.url.clone());
                *title_ref.lock().unwrap() = next.title.clone();
                *artist_ref.lock().unwrap() = next.artist.clone();
                if let Some(dur) = next.duration_ms {
                    duration_ms_arc.store(dur, Ordering::SeqCst);
                }
                // Reset position and seek offset for the new track.
                // The poller will see position drop from near-end to 0,
                // detect a gapless position reset, and call
                // advance_queue_metadata() — no stop/restart needed.
                seek_offset = 0;
                seek_offset_arc.store(0, Ordering::SeqCst);
                position_ms.store(0, Ordering::SeqCst);

                // Fetch the next track's HTTP stream
                let next_response = match crate::http::client::blocking_builder()
                    .timeout(None)
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .build()
                    .and_then(|client| client.get(&next.url).send())
                {
                    Ok(r) if r.status().is_success() || r.status().as_u16() == 206 => r,
                    Ok(r) => {
                        warn!(
                            status = %r.status(),
                            url = %next.url,
                            "local_audio_gapless_http_error"
                        );
                        break;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            url = %next.url,
                            "local_audio_gapless_http_fetch_failed"
                        );
                        break;
                    }
                };

                // Read header bytes from the next track.
                // The next track's transcode session may have only just
                // been started, so its very first read can time out before
                // the WAV header is available. Retry on TimedOut/WouldBlock —
                // mirroring the initial-track header read above — instead of
                // aborting the gapless chain, which would skip the track.
                let mut next_reader = next_response;
                let mut next_header = vec![0u8; 4096];
                let nh_read = loop {
                    if force_silent.load(Ordering::Relaxed) {
                        break 0;
                    }
                    match next_reader.read(&mut next_header) {
                        Ok(n) => break n,
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Stream not ready yet — wait for the producer.
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_header_read_failed");
                            break 0;
                        }
                    }
                };
                if nh_read == 0 {
                    warn!("local_audio_gapless_header_read_empty");
                    break;
                }
                next_header.truncate(nh_read);

                // Parse the WAV header of the next track
                let Some((new_ch, new_sr, new_bd, new_data_offset)) =
                    parse_wav_header(&next_header)
                else {
                    // Not a WAV stream — cannot chain gaplessly.
                    // Fall through to normal end-of-track handling.
                    info!("local_audio_gapless_next_not_wav_falling_back");
                    break;
                };

                info!(
                    new_sr,
                    new_ch,
                    new_bd,
                    prev_sr = sample_rate,
                    prev_ch = channels,
                    prev_bd = bit_depth,
                    "local_audio_gapless_next_track_format"
                );

                // Update source format variables for the new track
                let prev_sr = sample_rate;
                sample_rate = new_sr;
                channels = new_ch;
                bit_depth = new_bd;
                let new_bps = if new_bd == 0 {
                    4
                } else {
                    (new_bd / 8) as usize
                };
                frame_bytes = new_ch as usize * new_bps;
                needs_channel_adapt = output_ch != new_ch;
                needs_resample = output_sr != new_sr;

                // Recreate the resampler if the source sample rate changed
                if needs_resample && new_sr != prev_sr {
                    // Sample rate changed — flush old resampler residuals
                    resample_leftover.clear();
                    let ratio = output_sr as f64 / new_sr as f64;
                    let inv_ratio = 1.0 / ratio;
                    let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
                        (32_usize, 64_usize)
                    } else {
                        (64_usize, 128_usize)
                    };
                    let window = WindowFunction::BlackmanHarris2;
                    let f_cutoff = calculate_cutoff(sinc_len, window);
                    let params = SincInterpolationParameters {
                        sinc_len,
                        f_cutoff,
                        interpolation: SincInterpolationType::Linear,
                        oversampling_factor,
                        window,
                    };
                    resampler = match Async::<f32>::new_sinc(
                        ratio,
                        1.1,
                        &params,
                        1024,
                        output_ch as usize,
                        FixedAsync::Input,
                    ) {
                        Ok(r) => {
                            info!(
                                from_sr = new_sr,
                                to_sr = output_sr,
                                "local_audio_gapless_resampler_recreated"
                            );
                            Some(r)
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_resampler_failed");
                            needs_resample = false;
                            None
                        }
                    };
                    resample_leftover.clear();
                } else if needs_resample && new_sr == prev_sr {
                    if let Some(ref mut r) = resampler {
                        r.reset();
                    }
                    resample_leftover.clear();
                } else if !needs_resample && resampler.is_some() {
                    resampler = None;
                    resample_leftover.clear();
                }

                // Reset per-track counters
                total_frames_fed = 0;
                total_bytes_read = 0;
                leftover.clear();
                http_eof = false;

                // Process initial PCM data from the header read
                let gapless_pcm = if new_data_offset < next_header.len() {
                    next_header[new_data_offset..].to_vec()
                } else {
                    Vec::new()
                };
                if !gapless_pcm.is_empty() {
                    let aligned = (gapless_pcm.len() / frame_bytes) * frame_bytes;
                    if aligned > 0 {
                        let mut smp = pcm_bytes_to_f32(&gapless_pcm[..aligned], bit_depth);
                        if needs_channel_adapt {
                            smp = adapt_channels(&smp, channels, output_ch);
                        }
                        if needs_resample {
                            smp = rubato_resample_chunk(
                                &mut resampler,
                                &smp,
                                output_ch,
                                false,
                                &mut resample_leftover,
                            );
                        }
                        feed_ring_abortable(&ring, &smp, &stop_rx, &paused, Some(&force_silent));
                        total_frames_fed += (aligned / frame_bytes) as u64;
                    }
                    if aligned < gapless_pcm.len() {
                        leftover.extend_from_slice(&gapless_pcm[aligned..]);
                    }
                }

                // Main read loop for the gapless-chained track
                let mut gapless_read_buf = vec![0u8; 65536];
                loop {
                    if stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    match next_reader.read(&mut gapless_read_buf) {
                        Ok(0) => {
                            debug!(
                                total_bytes_read,
                                total_frames_fed, "local_audio_gapless_track_eof"
                            );
                            http_eof = true;
                            break;
                        }
                        Ok(n) => {
                            total_bytes_read += n as u64;
                            leftover.extend_from_slice(&gapless_read_buf[..n]);
                            let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                            if aligned == 0 {
                                continue;
                            }
                            let mut smp = pcm_bytes_to_f32(&leftover[..aligned], bit_depth);
                            let rem = leftover[aligned..].to_vec();
                            leftover = rem;
                            if needs_channel_adapt {
                                smp = adapt_channels(&smp, channels, output_ch);
                            }
                            if needs_resample {
                                smp = rubato_resample_chunk(
                                    &mut resampler,
                                    &smp,
                                    output_ch,
                                    false,
                                    &mut resample_leftover,
                                );
                            }
                            feed_ring_abortable(
                                &ring,
                                &smp,
                                &stop_rx,
                                &paused,
                                Some(&force_silent),
                            );
                            total_frames_fed += (aligned / frame_bytes) as u64;
                            let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0)
                                as u64
                                + seek_offset;
                            position_ms.store(pos, Ordering::Relaxed);
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_read_error");
                            break;
                        }
                    }
                }

                // If this track also reached clean EOF, loop back to check
                // for yet another gapless next track.  Otherwise, exit the
                // gapless loop and fall through to normal end handling.
                if !http_eof {
                    break;
                }
                info!("local_audio_gapless_track_finished_checking_next");
            }
            // ---------------------------------------------------------------
            // End of gapless continuation
            // ---------------------------------------------------------------

            // Flush the resampler: process any leftover frames + drain internal delay
            if needs_resample {
                let flushed = rubato_resample_chunk(
                    &mut resampler,
                    &[],
                    output_ch,
                    true,
                    &mut resample_leftover,
                );
                if !flushed.is_empty() {
                    feed_ring_abortable(&ring, &flushed, &stop_rx, &paused, Some(&force_silent));
                }
            }

            // Signal that HTTP reading is done
            finished_flag.store(true, Ordering::SeqCst);

            // Wait for the ring buffer to drain (real playback) before signalling
            // the natural track end. The HTTP thread finishes FEEDING all samples
            // well before the DAC has PLAYED them — up to ~2s at the output rate
            // (more when resampling 44.1→192). The old code signalled end + left
            // the reported position at the fed/decoded end BEFORE draining, so the
            // poller saw position past (DB) duration + margin while up to ~2s was
            // still queued in the ring, and advanced the queue early — cutting the
            // end of every track (JP Borderies, WASAPI/ASIO exclusive, VX248: log
            // showed ring_available ~1.4M f32 samples still queued at advance time).
            //
            // Fix: during the drain, report the PLAYED position (fed − what is
            // still queued in the ring) so the poller's position-past-end check
            // tracks real playback; only signal track_ended_naturally once the ring
            // is actually empty. If a new play/stop interrupts the drain
            // (force_silent/stop_rx), the queue already moved on (force_silent is
            // only set by a fresh play_url) — so we must NOT emit a natural end for
            // this superseded track.
            let fed_position_ms = position_ms.load(Ordering::Relaxed);
            let mut drained_naturally = false;
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                if force_silent.load(Ordering::Relaxed) {
                    break;
                }
                let remaining = ring.available();
                if remaining == 0 {
                    drained_naturally = true;
                    break;
                }
                // Report real playback: subtract the still-queued ring content
                // (interleaved f32 samples at the output rate/channels).
                if output_sr > 0 && output_ch > 0 {
                    let ring_ms =
                        (remaining as f64 / output_ch as f64 / output_sr as f64 * 1000.0) as u64;
                    position_ms.store(fed_position_ms.saturating_sub(ring_ms), Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if http_eof && drained_naturally {
                position_ms.store(fed_position_ms, Ordering::Relaxed);
                track_ended_naturally.store(true, Ordering::SeqCst);
                track_ended_generation.store(my_generation, Ordering::SeqCst);
                TRACK_END_NOTIFY.notify_one();
                debug!(
                    total_bytes_read,
                    total_frames_fed, "local_audio_track_ended_naturally_post_drain"
                );
            }

            drop(stream);
            if play_generation.load(Ordering::SeqCst) == my_generation {
                playing.store(false, Ordering::SeqCst);
            } else {
                debug!("local_audio_stale_thread_skipping_playing_false");
            }
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
                // Wait for the playback thread to exit. ASIO exclusive needs
                // the device fully released before reopening — use 2s timeout
                // instead of 500ms to avoid device contention on rapid seeks.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
                loop {
                    if handle.is_finished() {
                        let _ = handle.join();
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        // Detach — force_silent keeps the old callback silent
                        // so there is no audible overlap; the thread will exit
                        // on its own once the blocking read returns.
                        debug!("local_audio_stop_thread_detached — old stream exits in background");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            })
            .await;
        }
        self.playing.store(false, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
        self.seek_offset_ms.store(0, Ordering::SeqCst);
        self.duration_ms.store(0, Ordering::SeqCst);
        // Clear the natural-end flag and generation so stale signals from
        // the previous track do not affect the next track's end-detection cycle.
        self.track_ended_naturally.store(false, Ordering::SeqCst);
        self.track_ended_generation.store(0, Ordering::SeqCst);
        *self.next_media.lock().unwrap() = None;
        *self.current_uri.lock().unwrap() = None;
        *self.track_title.lock().unwrap() = None;
        *self.track_artist.lock().unwrap() = None;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        // The local output plays from an HTTP stream consumed sequentially,
        // so true seek requires the orchestrator to restart the stream.
        // Store the seek offset so the new stream (which starts counting
        // frames from 0) reports the correct absolute position.
        self.seek_offset_ms.store(position_ms, Ordering::SeqCst);
        self.position_ms.store(position_ms, Ordering::SeqCst);
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
        let duration_ms = self.duration_ms.load(Ordering::Relaxed);

        // When the playback thread has signalled natural end-of-stream
        // (track_ended_naturally=true) but is still alive (playing=true,
        // typically blocked in WASAPI's drop(stream)), report the track as
        // Playing with position past the end.  This causes the poller's
        // position_past_end path (TransportState::Playing branch) to fire
        // after POSITION_PAST_END_TICKS, triggering auto_next without
        // waiting for the thread to fully exit.
        //
        // Once the thread finishes and sets playing=false, this branch no
        // longer fires and the normal Stopped state is reported — allowing
        // the is_short_track fast-path in the poller's Stopped branch to
        // handle short tracks correctly.
        //
        // The flag is cleared by stop() and play_url() so it only applies
        // to the current track.
        if self.track_ended_naturally.load(Ordering::Relaxed)
            && self.playing.load(Ordering::Relaxed)
            && duration_ms > 0
            && self.track_ended_generation.load(Ordering::Relaxed)
                == self.play_generation.load(Ordering::Relaxed)
        {
            return Ok(OutputStatus {
                state: TransportState::Playing,
                position_ms: duration_ms.saturating_add(5000),
                duration_ms,
                volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
                muted: self.muted.load(Ordering::Relaxed),
                current_uri: self.current_uri.lock().unwrap().clone(),
                track_title: self.track_title.lock().unwrap().clone(),
                track_artist: self.track_artist.lock().unwrap().clone(),
                ended_naturally: true,
            });
        }

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
            duration_ms,
            volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri: self.current_uri.lock().unwrap().clone(),
            track_title: self.track_title.lock().unwrap().clone(),
            track_artist: self.track_artist.lock().unwrap().clone(),
            ended_naturally: self.track_ended_naturally.load(Ordering::Relaxed),
        })
    }

    async fn is_available(&self) -> bool {
        let name = self.device_name.clone();
        let backend = self.audio_backend.clone();
        // Probe on a blocking thread to avoid cpal blocking the async runtime
        tokio::task::spawn_blocking(move || {
            let host = select_host(&backend);
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

/// Find an audio output device by name, falling back to the default device if
/// the requested device is not found.
///
/// On macOS (and USB DACs in general), device IDs/names can change between
/// reboots, reconnections, or macOS audio routing changes.  When the stored
/// zone `device_name` no longer matches any enumerated device, playback would
/// silently fail with no audio output.  This function prevents that by falling
/// back to the system default output device and logging a clear warning.
///
/// Returns `(device, fell_back)` where `fell_back` is `true` if the default
/// device was used instead of the requested one.
fn find_device_with_fallback(host: &cpal::Host, device_name: &str) -> Option<(cpal::Device, bool)> {
    if device_name == "default" {
        return host.default_output_device().map(|d| (d, false));
    }

    // Try exact or substring match first (case-insensitive, bidirectional)
    let search = device_name.to_lowercase();
    let found = host.output_devices().ok().and_then(|mut devs| {
        devs.find(|d| {
            d.description()
                .map(|desc| {
                    let n = desc.name().to_string();
                    let lower = n.to_lowercase();
                    lower == search || lower.contains(&search) || search.contains(&lower)
                })
                .unwrap_or(false)
        })
    });

    if let Some(device) = found {
        return Some((device, false));
    }

    // Device not found — log available devices and fall back to default
    let available: Vec<String> = host
        .output_devices()
        .map(|devs| {
            devs.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
                .collect()
        })
        .unwrap_or_default();

    if let Some(default_device) = host.default_output_device() {
        let default_name = default_device
            .description()
            .map(|desc| desc.name().to_string())
            .unwrap_or_else(|_| "unknown".into());
        warn!(
            requested = %device_name,
            fallback = %default_name,
            available = ?available,
            "audio_device_not_found_falling_back_to_default — \
             the configured device is unavailable (unplugged, renamed, or \
             macOS audio routing changed); using the system default output \
             device instead"
        );
        Some((default_device, true))
    } else {
        warn!(
            requested = %device_name,
            available = ?available,
            "audio_device_not_found_no_default_available"
        );
        None
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
/// Probe a device's capabilities when `supported_output_configs()` is
/// unavailable. Returns `(max_channels, sample_rates, caps_reliable)`.
///
/// `caps_reliable` is true when the caps came from the device's real default
/// config, false when they are the last-resort assumed stereo guess. Callers
/// must NOT collapse two devices as duplicates on unreliable caps: a generic
/// "Haut-Parleurs" USB DAC and the onboard output both fall to the same assumed
/// `(2, [44100,48000])` on Windows, and collapsing would wrongly drop the DAC
/// (Alain, #1084).
fn probe_device_fallback_caps(device: &cpal::Device, name: &str) -> (u16, Vec<u32>, bool) {
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
        (ch, rates, true)
    } else {
        // Last resort: assume stereo 44100/48000.  PipeWire will accept
        // these through its ALSA PCM plugin even without enumeration.
        info!(
            device = %name,
            "local_audio_device_fallback_to_assumed_stereo_44100_48000"
        );
        (2, vec![44100, 48000], false)
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

/// Resample a complete buffer of interleaved f32 samples using rubato sinc.
///
/// Used for compressed streams where all decoded data is available at once.
/// Creates and consumes a temporary resampler internally.
fn rubato_resample_batch(samples: &[f32], from_sr: u32, to_sr: u32, channels: u16) -> Vec<f32> {
    if from_sr == to_sr || samples.is_empty() {
        return samples.to_vec();
    }
    let ch = channels as usize;
    if ch == 0 {
        return Vec::new();
    }

    let ratio = to_sr as f64 / from_sr as f64;
    // Adaptive resampler params based on conversion ratio:
    //   ratio ≤ 2.0 (e.g. 96kHz→48kHz): quality params, plenty of CPU budget
    //   ratio > 2.0 (e.g. 176.4kHz→48kHz, 192kHz→48kHz): lighter params
    //     to avoid real-time stuttering on Windows (still ~90dB SNR)
    let inv_ratio = 1.0 / ratio; // > 1.0 when downsampling
    let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
        (32_usize, 64_usize) // lighter: 176.4/192kHz → 48kHz
    } else {
        (64_usize, 128_usize) // standard: 96kHz → 48kHz
    };
    let window = WindowFunction::BlackmanHarris2;
    let f_cutoff = calculate_cutoff(sinc_len, window);
    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor,
        window,
    };
    let mut resampler =
        match Async::<f32>::new_sinc(ratio, 1.1, &params, 1024, ch, FixedAsync::Input) {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(error = %e, "rubato_batch_resampler_creation_failed_using_linear");
                return simple_resample(samples, from_sr, to_sr, channels);
            }
        };

    // Resample using the chunk helper, then flush
    let mut batch_leftover: Vec<f32> = Vec::new();
    let mut out = rubato_resample_chunk(
        &mut resampler,
        samples,
        channels,
        false,
        &mut batch_leftover,
    );
    let flushed = rubato_resample_chunk(&mut resampler, &[], channels, true, &mut batch_leftover);
    out.extend_from_slice(&flushed);

    info!(
        from_sr,
        to_sr,
        in_samples = samples.len(),
        out_samples = out.len(),
        "rubato_batch_resample_complete"
    );

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
    resample_leftover: &mut Vec<f32>,
) -> Vec<f32> {
    use rubato::audioadapter_buffers::direct::InterleavedSlice;
    use rubato::audioadapter_buffers::owned::InterleavedOwned;

    let Some(resampler) = resampler.as_mut() else {
        // No resampler available — pass through unchanged
        return samples.to_vec();
    };

    let ch = channels as usize;
    if ch == 0 {
        return Vec::new();
    }

    // Combine leftover from previous call with new samples.
    // This avoids using rubato's partial_len during continuous streaming,
    // which pads the remainder with silence and corrupts subsequent output
    // (perceived as white noise on 24-bit audio where frame counts rarely
    // align to the resampler's block size).
    let combined: Vec<f32>;
    let input_ref: &[f32] = if flush {
        // When flushing, drain leftover first, then feed silence
        if !resample_leftover.is_empty() {
            combined = resample_leftover.drain(..).collect();
            &combined
        } else {
            &[]
        }
    } else {
        if resample_leftover.is_empty() {
            // Fast path: no leftover, use new samples directly
            let usable = (samples.len() / ch) * ch;
            &samples[..usable]
        } else {
            // Prepend leftover from previous call
            combined = resample_leftover
                .drain(..)
                .chain(samples.iter().copied())
                .collect();
            let usable = (combined.len() / ch) * ch;
            // Any sub-frame remainder goes back to leftover (shouldn't happen
            // since both leftover and samples are frame-aligned, but be safe)
            if usable < combined.len() {
                resample_leftover.extend_from_slice(&combined[usable..]);
            }
            &combined[..usable]
        }
    };

    let actual_in_frames = input_ref.len() / ch;

    // Process only complete resampler blocks (input_frames_next() frames each).
    // Carry over any remaining frames to the next call instead of using
    // partial_len, which pads with silence and introduces artifacts.
    let mut all_output = Vec::new();
    let mut offset = 0;

    while offset < actual_in_frames {
        let chunk_needed = resampler.input_frames_next();
        let chunk_available = actual_in_frames - offset;

        if chunk_available < chunk_needed {
            if flush {
                // End of track: process remaining frames with silence padding
                let chunk_slice = &input_ref[offset * ch..actual_in_frames * ch];
                let input_adapter = match InterleavedSlice::new(chunk_slice, ch, chunk_available) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(error = %e, "rubato_input_adapter_error_flush");
                        break;
                    }
                };
                let out_frames = resampler.output_frames_next();
                let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);
                let indexing = rubato::Indexing {
                    input_offset: 0,
                    output_offset: 0,
                    partial_len: Some(chunk_available),
                    active_channels_mask: None,
                };
                match resampler.process_into_buffer(
                    &input_adapter,
                    &mut output_buf,
                    Some(&indexing),
                ) {
                    Ok((_nbr_in, nbr_out)) => {
                        let out_data = output_buf.take_data();
                        all_output.extend_from_slice(&out_data[..nbr_out * ch]);
                    }
                    Err(e) => {
                        warn!(error = %e, "rubato_process_error_flush");
                    }
                }
                offset = actual_in_frames;
            } else {
                // Continuous streaming: save remainder for next call
                resample_leftover.extend_from_slice(&input_ref[offset * ch..actual_in_frames * ch]);
                break;
            }
        } else {
            // Full block available — process without partial_len
            let chunk_slice = &input_ref[offset * ch..(offset + chunk_needed) * ch];
            let input_adapter = match InterleavedSlice::new(chunk_slice, ch, chunk_needed) {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "rubato_input_adapter_error");
                    break;
                }
            };

            let out_frames = resampler.output_frames_next();
            let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);

            match resampler.process_into_buffer(&input_adapter, &mut output_buf, None) {
                Ok((_nbr_in, nbr_out)) => {
                    let out_data = output_buf.take_data();
                    all_output.extend_from_slice(&out_data[..nbr_out * ch]);
                }
                Err(e) => {
                    warn!(error = %e, "rubato_process_error");
                    break;
                }
            }

            offset += chunk_needed;
        }
    }

    // If flushing and we processed all leftover above, now feed a block of
    // pure silence to drain the resampler's internal delay line.
    if flush && offset >= actual_in_frames {
        let silence_frames = resampler.input_frames_next();
        let silence = vec![0.0f32; silence_frames * ch];
        let input_adapter = match InterleavedSlice::new(&silence, ch, silence_frames) {
            Ok(a) => a,
            Err(_) => return all_output,
        };
        let out_frames = resampler.output_frames_next();
        let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);
        let indexing = rubato::Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(0),
            active_channels_mask: None,
        };
        if let Ok((_nbr_in, nbr_out)) =
            resampler.process_into_buffer(&input_adapter, &mut output_buf, Some(&indexing))
        {
            let out_data = output_buf.take_data();
            all_output.extend_from_slice(&out_data[..nbr_out * ch]);
        }
    }

    all_output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_read_retries_only_transient_kinds() {
        use std::io::ErrorKind;
        // #522: the next track's transcode hasn't emitted its WAV header yet →
        // retry instead of abandoning the gapless chain (would skip track 2).
        assert!(header_read_should_retry(ErrorKind::TimedOut));
        assert!(header_read_should_retry(ErrorKind::WouldBlock));
        // Real errors still fail fast (no infinite retry on a dead stream).
        assert!(!header_read_should_retry(ErrorKind::BrokenPipe));
        assert!(!header_read_should_retry(ErrorKind::UnexpectedEof));
        assert!(!header_read_should_retry(ErrorKind::NotFound));
    }

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
    fn test_ring_buffer_clear() {
        let ring = RingBuf::new(16);
        let data = [1.0f32, 2.0, 3.0, 4.0];
        ring.push(&data);
        assert_eq!(ring.available(), 4);

        ring.clear();
        assert_eq!(ring.available(), 0);

        // After clear, reading should return zeros
        let mut out = [0.0f32; 4];
        ring.push(&[5.0, 6.0]);
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out[0], 5.0);
        assert_eq!(out[1], 6.0);
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
    fn test_resample_chunk_no_silence_padding() {
        // Verify that rubato_resample_chunk does NOT use partial_len (silence
        // padding) during continuous streaming.  This was the root cause of
        // white noise on 24-bit audio: frame counts from HTTP reads rarely
        // aligned to the resampler's block size (1024), so every chunk had a
        // trailing partial block padded with silence.
        use rubato::{
            Async, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
            calculate_cutoff,
        };

        let ch = 2usize;
        let ratio = 48000.0 / 96000.0; // downsample 2:1
        let sinc_len = 64;
        let window = WindowFunction::BlackmanHarris2;
        let f_cutoff = calculate_cutoff(sinc_len, window);
        let params = SincInterpolationParameters {
            sinc_len,
            f_cutoff,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window,
        };
        let mut resampler: Option<Async<f32>> =
            Some(Async::new_sinc(ratio, 1.1, &params, 1024, ch, FixedAsync::Input).unwrap());

        let mut resample_leftover: Vec<f32> = Vec::new();

        // Simulate two chunks of 683 frames (not aligned to 1024 block size).
        // This is what happens with 24-bit stereo: 65536 bytes / 6 = ~10922 frames,
        // 10922 % 1024 = 682 remainder.  Here we use a single remainder-sized chunk.
        let chunk1: Vec<f32> = (0..683 * ch).map(|i| (i as f32 * 0.001).sin()).collect();
        let chunk2: Vec<f32> = (0..683 * ch).map(|i| (i as f32 * 0.002).sin()).collect();

        // First call: 683 frames < 1024 block size, so all go to leftover
        let out1 = rubato_resample_chunk(
            &mut resampler,
            &chunk1,
            ch as u16,
            false,
            &mut resample_leftover,
        );
        // No output yet (not enough frames for a complete block)
        assert!(
            out1.is_empty(),
            "expected no output from first partial chunk, got {} samples",
            out1.len()
        );
        assert_eq!(
            resample_leftover.len(),
            683 * ch,
            "leftover should hold all 683 frames"
        );

        // Second call: leftover (683) + new (683) = 1366 frames >= 1024
        let out2 = rubato_resample_chunk(
            &mut resampler,
            &chunk2,
            ch as u16,
            false,
            &mut resample_leftover,
        );
        // Should have output from 1 complete block (1024 input -> ~512 output frames)
        assert!(
            !out2.is_empty(),
            "expected output after accumulating enough frames"
        );
        // Leftover should have 1366 - 1024 = 342 frames
        assert_eq!(resample_leftover.len(), 342 * ch);

        // Flush: process remaining 342 frames with silence padding
        let flushed =
            rubato_resample_chunk(&mut resampler, &[], ch as u16, true, &mut resample_leftover);
        assert!(
            !flushed.is_empty(),
            "flush should produce output from remaining frames"
        );
        assert!(
            resample_leftover.is_empty(),
            "leftover should be empty after flush"
        );

        // Verify no NaN or infinity in output
        for s in out2.iter().chain(flushed.iter()) {
            assert!(s.is_finite(), "output contains non-finite value: {}", s);
        }
    }

    #[test]
    fn test_list_audio_devices() {
        // Should not panic, even if no devices available
        let devices = list_audio_devices();
        // On CI there may be no devices, but on dev machines there should be at least one
        let _ = devices.len();
    }
}
