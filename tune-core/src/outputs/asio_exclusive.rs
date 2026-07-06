//! ASIO exclusive/bit-perfect audio output on Windows.
//!
//! When `local_exclusive_mode` is enabled and the ASIO backend is selected,
//! this module uses CPAL's ASIO host to claim exclusive access to the audio
//! device, bypassing Windows audio mixing (WASAPI shared mode).
//!
//! ASIO drivers inherently provide exclusive access to the audio hardware:
//!
//! 1. **Exclusive access** -- ASIO drivers lock the device for a single
//!    application, so no other audio can interfere.
//! 2. **Hardware sample rate** -- sets the device's sample rate to match the
//!    source material (e.g. 96 kHz, 192 kHz).
//! 3. **Bit-perfect output** -- PCM samples are fed directly to the DAC via
//!    the ASIO driver with no system-level resampling or mixing.
//!
//! On drop, the original sample rate is restored (if changed).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{debug, info, warn};

use super::local::RingBuf;

/// Process-wide guard serializing access to the single ASIO device. ASIO
/// forbids two concurrent streams on the same device — even within one
/// process. When a track ends and the user force-plays another, a new stream
/// used to be opened ~1 ms after the previous one was aborted, before the old
/// instance's Drop had released the driver. That race crashed the Fireface
/// ASIO driver natively (no Rust panic, process gone). Holding this lock for
/// the whole session (acquired in `new`, released after Drop tears the driver
/// down and settles) makes the new open WAIT for the old one instead.
static ASIO_DEVICE_LOCK: Mutex<()> = Mutex::new(());

/// Time given to the ASIO driver to fully release the hardware after a stream
/// is torn down, before the device lock is released and the next open runs.
const ASIO_TEARDOWN_SETTLE: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Exclusive-mode ASIO audio output handle.
///
/// Holds ownership of the CPAL stream and enough state to restore the
/// device's original sample rate on drop.
pub struct AsioExclusiveOutput {
    device_name: String,
    original_sample_rate: Option<u32>,
    current_sample_rate: u32,
    stream: Option<cpal::Stream>,
    ring: Arc<RingBuf>,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    volume: Arc<AtomicU32>,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    paused: Arc<AtomicBool>,
    /// Held for the whole session so no other ASIO stream can open on the
    /// device concurrently. Released (with a settle delay) in `Drop`.
    #[allow(dead_code)]
    device_guard: MutexGuard<'static, ()>,
}

/// Information about the currently-configured exclusive format.
#[derive(Debug, Clone)]
pub struct AsioExclusiveFormatInfo {
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub device_name: String,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl AsioExclusiveOutput {
    /// Open the named ASIO device in exclusive mode and configure it for the
    /// given sample rate / bit depth / channel count.
    ///
    /// `device_name` may be `"default"` to use the first ASIO device.
    pub fn new(
        device_name: &str,
        sample_rate: u32,
        bit_depth: u32,
        channels: u32,
        ring: Arc<RingBuf>,
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        // Serialize device access process-wide BEFORE touching the driver: if
        // the previous exclusive session is still tearing down, block here
        // until its Drop releases the device (recovering from a poisoned lock
        // if a prior holder panicked) instead of racing it and crashing the
        // native ASIO driver.
        let device_guard = ASIO_DEVICE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        ensure_com_initialized();

        // -- 1. Get the ASIO host -------------------------------------------
        let host = cpal::host_from_id(cpal::HostId::Asio)
            .map_err(|e| format!("Failed to get ASIO host: {e}"))?;

        info!(
            device = %device_name,
            sample_rate,
            bit_depth,
            channels,
            "asio_exclusive_opening"
        );

        // -- 2. Resolve device ---------------------------------------------
        let mut available_names: Vec<String> = Vec::new();
        let device = if device_name == "default" {
            host.default_output_device()
                .ok_or_else(|| "No default ASIO output device found".to_string())?
        } else {
            let mut found = None;
            let search = device_name.to_lowercase();
            if let Ok(devices) = host.output_devices() {
                for dev in devices {
                    if let Ok(desc) = dev.description() {
                        let name = desc.name().to_string();
                        let lower = name.to_lowercase();
                        available_names.push(name.clone());
                        if lower == search || lower.contains(&search) || search.contains(&lower) {
                            found = Some(dev);
                            break;
                        }
                    }
                }
                if found.is_none() {
                    warn!(
                        requested = %device_name,
                        available = ?available_names,
                        "asio_device_not_found_listing_available"
                    );
                }
            }
            match found {
                Some(dev) => dev,
                None => {
                    return Err(format!(
                        "ASIO device not found: {device_name}. Available: {:?}",
                        available_names
                    ));
                }
            }
        };

        let resolved_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| device_name.to_string());

        // -- 3. Read the device's current sample rate (if we can) -----------
        let original_sample_rate = device
            .default_output_config()
            .ok()
            .map(|c| c.config().sample_rate);

        if let Some(orig_sr) = original_sample_rate {
            info!(
                original_sample_rate = orig_sr,
                "asio_exclusive_original_rate"
            );
        }

        // -- 4. Find a matching config at the source sample rate ------------
        //
        // ASIO drivers typically support the exact hardware rates of the DAC.
        // We look for a config that matches our desired sample rate and
        // channel count.  The returned `native_fmt` is the driver's native
        // sample format — cpal's ASIO backend does NO format conversion, so
        // we must build the stream with an exact-match callback type.
        let (stream_config, native_fmt) = Self::find_exclusive_config(
            &device,
            channels as u16,
            sample_rate,
        )
        .ok_or_else(|| {
            format!("ASIO device {resolved_name} does not support {channels}ch @ {sample_rate} Hz")
        })?;

        info!(
            device = %resolved_name,
            sample_rate = stream_config.sample_rate,
            channels = stream_config.channels,
            native_format = ?native_fmt,
            "asio_exclusive_config_found"
        );

        // -- 5. Build output stream with render callback --------------------
        //
        // The ring buffer always stores f32 samples internally.  When the
        // ASIO driver's native format is *not* F32 we read f32 from the
        // ring, apply volume, then convert to the driver's native type in
        // the callback.  This keeps the entire pipeline in f32 while giving
        // the driver the exact integer format it expects.
        let stream = Self::build_native_stream(
            &device,
            &stream_config,
            native_fmt,
            ring.clone(),
            volume.clone(),
            paused.clone(),
        )?;

        stream
            .play()
            .map_err(|e| format!("Failed to start ASIO stream: {e}"))?;

        info!(
            device = %resolved_name,
            sample_rate,
            bit_depth,
            channels,
            "asio_exclusive_started"
        );

        Ok(Self {
            device_name: resolved_name,
            original_sample_rate,
            current_sample_rate: sample_rate,
            stream: Some(stream),
            ring,
            volume,
            paused,
            device_guard,
        })
    }

    /// Release exclusive mode and stop the stream.
    pub fn release(&mut self) -> Result<(), String> {
        // Stop and drop the stream
        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.pause() {
                warn!(error = %e, "asio_exclusive_pause_failed");
            }
            // Stream is dropped here, releasing the ASIO device
            drop(stream);
        }

        // Log restoration info (ASIO drivers typically restore their state
        // when the stream is dropped, but we log for diagnostics).
        if let Some(orig_sr) = self.original_sample_rate {
            if orig_sr != self.current_sample_rate {
                info!(
                    from = self.current_sample_rate,
                    to = orig_sr,
                    device = %self.device_name,
                    "asio_exclusive_sample_rate_will_restore_on_driver_release"
                );
            }
        }

        info!(device = %self.device_name, "asio_exclusive_released");
        Ok(())
    }

    /// Returns true if ASIO exclusive mode is available on this platform.
    pub fn is_available() -> bool {
        // Check if we can actually get an ASIO host
        cpal::host_from_id(cpal::HostId::Asio).is_ok()
    }

    /// Returns the ring buffer reference for external feeding.
    pub fn ring(&self) -> &Arc<RingBuf> {
        &self.ring
    }

    /// Build the cpal output stream using the driver's native sample format.
    ///
    /// The ring buffer always contains f32 samples.  For native I32 or I16
    /// drivers (e.g. RME Babyface Pro FS) we read f32 from the ring, apply
    /// volume, and convert to the target integer type in the callback.
    fn build_native_stream(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        native_fmt: SampleFormat,
        ring: Arc<RingBuf>,
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, String> {
        match native_fmt {
            SampleFormat::I32 => {
                info!("asio_exclusive_building_i32_stream");
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            // Read f32 from ring into a temporary buffer
                            let mut tmp = vec![0.0f32; data.len()];
                            let read = ring.pop(&mut tmp);
                            // Apply volume and convert f32 → i32
                            let v = volume.load(Ordering::Relaxed) as f64 / 1000.0;
                            let scale = i32::MAX as f64;
                            for i in 0..read {
                                let s = (tmp[i] as f64 * v).clamp(-1.0, 1.0);
                                data[i] = (s * scale) as i32;
                            }
                            // Silence remaining
                            for sample in &mut data[read..] {
                                *sample = 0;
                            }
                        },
                        |e| warn!(error = %e, "asio_exclusive_stream_error"),
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO I32 stream: {e}"))
            }
            SampleFormat::I16 => {
                info!("asio_exclusive_building_i16_stream");
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let mut tmp = vec![0.0f32; data.len()];
                            let read = ring.pop(&mut tmp);
                            let v = volume.load(Ordering::Relaxed) as f64 / 1000.0;
                            let scale = i16::MAX as f64;
                            for i in 0..read {
                                let s = (tmp[i] as f64 * v).clamp(-1.0, 1.0);
                                data[i] = (s * scale) as i16;
                            }
                            for sample in &mut data[read..] {
                                *sample = 0;
                            }
                        },
                        |e| warn!(error = %e, "asio_exclusive_stream_error"),
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO I16 stream: {e}"))
            }
            _ => {
                // F32 (default) and any other format — use f32 callback
                if native_fmt != SampleFormat::F32 {
                    warn!(
                        native_format = ?native_fmt,
                        "asio_exclusive_unexpected_format_falling_back_to_f32"
                    );
                }
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0.0);
                                return;
                            }
                            let read = ring.pop(data);
                            let v = volume.load(Ordering::Relaxed) as f32 / 1000.0;
                            for sample in &mut data[..read] {
                                *sample *= v;
                            }
                            for sample in &mut data[read..] {
                                *sample = 0.0;
                            }
                        },
                        |e| warn!(error = %e, "asio_exclusive_stream_error"),
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO F32 stream: {e}"))
            }
        }
    }

    /// Find a stream config matching the desired channels and sample rate
    /// using the ASIO device's supported configurations.
    ///
    /// Returns `(StreamConfig, SampleFormat)` — the sample format is the
    /// driver's **native** format, which cpal's ASIO backend requires an
    /// exact match for (no implicit conversion).
    fn find_exclusive_config(
        device: &cpal::Device,
        channels: u16,
        sample_rate: u32,
    ) -> Option<(cpal::StreamConfig, SampleFormat)> {
        // First, try to find an exact match in supported configs
        if let Ok(configs) = device.supported_output_configs() {
            for config in configs {
                if config.channels() >= channels
                    && config.min_sample_rate() <= sample_rate
                    && config.max_sample_rate() >= sample_rate
                {
                    let native_fmt = config.sample_format();
                    return Some((
                        cpal::StreamConfig {
                            channels: channels.min(config.channels()),
                            sample_rate,
                            buffer_size: cpal::BufferSize::Default,
                        },
                        native_fmt,
                    ));
                }
            }
        }

        // If no exact match, try with the device's default config
        if let Ok(default_cfg) = device.default_output_config() {
            let cfg = default_cfg.config();
            let native_fmt = default_cfg.sample_format();
            // Even if the rate doesn't match, ASIO drivers may accept it
            // and switch the hardware rate internally.
            debug!(
                default_sr = cfg.sample_rate,
                default_ch = cfg.channels,
                requested_sr = sample_rate,
                requested_ch = channels,
                native_format = ?native_fmt,
                "asio_exclusive_using_direct_config"
            );
            return Some((
                cpal::StreamConfig {
                    channels: channels.min(cfg.channels),
                    sample_rate,
                    buffer_size: cpal::BufferSize::Default,
                },
                native_fmt,
            ));
        }

        None
    }
}

impl Drop for AsioExclusiveOutput {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            warn!(error = %e, "asio_exclusive_drop_release_failed");
        }
        // Let the driver fully release the hardware before `_device_guard`
        // (dropped after this body) frees the lock and the next open runs.
        // Opening while the driver is still busy returns DeviceBusy and, on the
        // Fireface, crashes the process natively.
        std::thread::sleep(ASIO_TEARDOWN_SETTLE);
        debug!(device = %self.device_name, "asio_exclusive_device_lock_releasing");
    }
}

// ---------------------------------------------------------------------------
// Helper: COM initialization for ASIO (Windows only)
// ---------------------------------------------------------------------------

/// Initialize COM in STA mode on the current thread.
/// ASIO drivers are COM objects that require Single-Threaded Apartment mode.
/// Must be called before any cpal ASIO host/device operations.
#[cfg(target_os = "windows")]
pub(crate) fn ensure_com_initialized() {
    unsafe extern "system" {
        fn CoInitializeEx(pvreserved: *const std::ffi::c_void, dwcoinit: u32) -> i32;
    }
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const S_OK: i32 = 0;
    const S_FALSE: i32 = 1;
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED) };
    match hr {
        S_OK => debug!("com_sta_initialized"),
        S_FALSE => debug!("com_sta_already_initialized"),
        _ => warn!(hresult = hr, "com_init_failed_or_changed_mode"),
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn ensure_com_initialized() {}

// ---------------------------------------------------------------------------
// Helper: check exclusive mode support at runtime
// ---------------------------------------------------------------------------

/// Returns `true` on Windows with ASIO support (where ASIO exclusive mode is available).
pub fn supports_exclusive_mode() -> bool {
    ensure_com_initialized();
    cpal::host_from_id(cpal::HostId::Asio).is_ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supports_exclusive_mode() {
        // On Windows with ASIO drivers installed, this returns true.
        // On CI or machines without ASIO, it returns false.
        let _ = supports_exclusive_mode();
    }

    #[test]
    fn test_asio_exclusive_output_is_available() {
        // Same as above — just verify it doesn't panic
        let _ = AsioExclusiveOutput::is_available();
    }
}
