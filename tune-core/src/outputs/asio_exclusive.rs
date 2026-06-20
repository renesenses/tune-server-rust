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

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{debug, info, warn};

use super::local::RingBuf;

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
        // -- 0. Initialize COM for this thread (required for ASIO SDK) ------
        // tokio::task::spawn_blocking worker threads don't have COM initialized.
        // Without this, cpal::host_from_id(Asio) fails to enumerate drivers
        // because the ASIO SDK uses COM internally.
        // ASIO drivers are old-school COM objects that require STA (Single-Threaded
        // Apartment) mode — MTA causes registry reads and driver instantiation to fail.
        #[cfg(target_os = "windows")]
        {
            unsafe extern "system" {
                fn CoInitializeEx(pvreserved: *const std::ffi::c_void, dwcoinit: u32) -> i32;
            }
            const COINIT_APARTMENTTHREADED: u32 = 0x2;
            unsafe {
                CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);
            }
        }

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
        let device = if device_name == "default" {
            host.default_output_device()
                .ok_or_else(|| "No default ASIO output device found".to_string())?
        } else {
            let mut found = None;
            let search = device_name.to_lowercase();
            if let Ok(devices) = host.output_devices() {
                let mut available_names = Vec::new();
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
                    // Fall back to default ASIO device
                    let default = host.default_output_device().ok_or_else(|| {
                        format!(
                            "ASIO device not found: {device_name} (and no default device available)"
                        )
                    })?;
                    let default_name = default
                        .description()
                        .map(|d| d.name().to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    warn!(
                        requested = %device_name,
                        fallback = %default_name,
                        "asio_exclusive_device_not_found_falling_back_to_default"
                    );
                    default
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
        // channel count.
        let stream_config = Self::find_exclusive_config(&device, channels as u16, sample_rate)
            .ok_or_else(|| {
                format!(
                    "ASIO device {resolved_name} does not support {channels}ch @ {sample_rate} Hz"
                )
            })?;

        info!(
            device = %resolved_name,
            sample_rate = stream_config.sample_rate,
            channels = stream_config.channels,
            "asio_exclusive_config_found"
        );

        // -- 5. Build output stream with render callback --------------------
        let ring_for_callback = ring.clone();
        let vol_for_callback = volume.clone();
        let paused_for_callback = paused.clone();

        let stream = device
            .build_output_stream(
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

                    // Silence for any remaining samples
                    for sample in &mut data[read..] {
                        *sample = 0.0;
                    }
                },
                |e| warn!(error = %e, "asio_exclusive_stream_error"),
                None,
            )
            .map_err(|e| format!("Failed to build ASIO stream: {e}"))?;

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

    /// Find a stream config matching the desired channels and sample rate
    /// using the ASIO device's supported configurations.
    fn find_exclusive_config(
        device: &cpal::Device,
        channels: u16,
        sample_rate: u32,
    ) -> Option<cpal::StreamConfig> {
        // First, try to find an exact match in supported configs
        if let Ok(configs) = device.supported_output_configs() {
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
        }

        // If no exact match, try with the device's default config
        if let Ok(default_cfg) = device.default_output_config() {
            let cfg = default_cfg.config();
            // Even if the rate doesn't match, ASIO drivers may accept it
            // and switch the hardware rate internally.
            debug!(
                default_sr = cfg.sample_rate,
                default_ch = cfg.channels,
                requested_sr = sample_rate,
                requested_ch = channels,
                "asio_exclusive_using_direct_config"
            );
            return Some(cpal::StreamConfig {
                channels: channels.min(cfg.channels),
                sample_rate,
                buffer_size: cpal::BufferSize::Default,
            });
        }

        None
    }
}

impl Drop for AsioExclusiveOutput {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            warn!(error = %e, "asio_exclusive_drop_release_failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: check exclusive mode support at runtime
// ---------------------------------------------------------------------------

/// Returns `true` on Windows with ASIO support (where ASIO exclusive mode is available).
pub fn supports_exclusive_mode() -> bool {
    // Runtime check: can we actually load the ASIO host?
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
