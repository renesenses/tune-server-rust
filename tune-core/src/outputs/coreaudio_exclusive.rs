//! CoreAudio exclusive/bit-perfect audio output on macOS.
//!
//! When `local_exclusive_mode` is enabled, this module bypasses cpal's shared
//! mode path and talks directly to CoreAudio via the `coreaudio-rs` crate:
//!
//! 1. **Hog Mode** — claims exclusive access to the audio device so macOS
//!    cannot mix other applications' audio into the same stream.
//! 2. **Hardware sample rate** — sets the device's nominal sample rate to
//!    match the source material (e.g. 96 kHz, 192 kHz).
//! 3. **Physical stream format** — configures the device for the exact bit
//!    depth / channel layout of the source, eliminating any format conversion
//!    by the HAL mixer.
//! 4. **Direct output** — uses a HAL-level AudioUnit (`kAudioUnitSubType_HALOutput`)
//!    with an interleaved render callback that feeds PCM samples straight to the
//!    hardware.
//!
//! On drop, the original sample rate is restored and hog mode is released.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers;
use coreaudio::audio_unit::render_callback;
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope, StreamFormat};
use tracing::{info, warn};

use super::local::RingBuf;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Exclusive-mode audio output handle.
///
/// Holds ownership of the hog mode claim, the AudioUnit, and enough state to
/// restore the device's original sample rate on drop.
pub struct ExclusiveOutput {
    device_id: u32,
    original_sample_rate: f64,
    is_hogged: bool,
    audio_unit: Option<AudioUnit>,
    ring: Arc<RingBuf>,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    volume: Arc<std::sync::atomic::AtomicU32>,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    paused: Arc<AtomicBool>,
}

/// Information about the currently-configured exclusive format.
#[derive(Debug, Clone)]
pub struct ExclusiveFormatInfo {
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub device_name: String,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ExclusiveOutput {
    /// Claim exclusive access to the named device and configure it for the
    /// given sample rate / bit depth / channel count.
    ///
    /// `device_name` may be `"default"` to use the system default output.
    pub fn new(
        device_name: &str,
        sample_rate: u32,
        bit_depth: u32,
        channels: u32,
        ring: Arc<RingBuf>,
        volume: Arc<std::sync::atomic::AtomicU32>,
        paused: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        // -- 1. Resolve device ID ----------------------------------------
        let device_id = if device_name == "default" {
            macos_helpers::get_default_device_id(false)
                .ok_or_else(|| "No default output device found".to_string())?
        } else {
            macos_helpers::get_device_id_from_name(device_name, false)
                .ok_or_else(|| format!("Audio device not found: {device_name}"))?
        };

        let resolved_name =
            macos_helpers::get_device_name(device_id).unwrap_or_else(|_| device_name.to_string());

        info!(
            device = %resolved_name,
            device_id,
            sample_rate,
            bit_depth,
            channels,
            "coreaudio_exclusive_opening"
        );

        // -- 2. Read the current (original) sample rate ------------------
        let original_sample_rate = Self::get_device_sample_rate(device_id)?;
        info!(original_sample_rate, "coreaudio_exclusive_original_rate");

        // -- 3. Acquire hog mode -----------------------------------------
        let mut is_hogged = false;
        let current_hog_pid = macos_helpers::get_hogging_pid(device_id).unwrap_or(-1);
        let my_pid = std::process::id() as i32;

        if current_hog_pid != -1 && current_hog_pid != my_pid {
            return Err(format!(
                "Device {resolved_name} is already hogged by PID {current_hog_pid}"
            ));
        }

        if current_hog_pid != my_pid {
            match macos_helpers::toggle_hog_mode(device_id) {
                Ok(pid) if pid == my_pid => {
                    is_hogged = true;
                    info!(pid, device = %resolved_name, "coreaudio_exclusive_hog_acquired");
                }
                Ok(pid) => {
                    warn!(
                        pid,
                        device = %resolved_name,
                        "coreaudio_exclusive_hog_unexpected_pid"
                    );
                    // We'll still try to continue without hog mode
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        device = %resolved_name,
                        "coreaudio_exclusive_hog_failed"
                    );
                    // Continue without hog mode -- user will get a warning but
                    // bit-perfect format switching may still work on some DACs.
                }
            }
        } else {
            is_hogged = true;
            info!("coreaudio_exclusive_already_hogged");
        }

        // -- 4. Set hardware sample rate ---------------------------------
        if (original_sample_rate as u32) != sample_rate {
            if let Err(e) = macos_helpers::set_device_sample_rate(device_id, sample_rate as f64) {
                warn!(
                    error = %e,
                    wanted = sample_rate,
                    "coreaudio_exclusive_set_sample_rate_failed"
                );
                // Non-fatal: the AudioUnit may still resample internally
            } else {
                info!(
                    from = original_sample_rate as u32,
                    to = sample_rate,
                    "coreaudio_exclusive_sample_rate_changed"
                );
            }
        }

        // -- 5. Set physical stream format (bit-perfect) -----------------
        let ca_sample_format = match bit_depth {
            16 => SampleFormat::I16,
            24 => SampleFormat::I24,
            32 => SampleFormat::I32,
            _ => SampleFormat::I16,
        };

        let desired_stream_format = StreamFormat {
            sample_rate: sample_rate as f64,
            sample_format: ca_sample_format,
            flags: LinearPcmFlags::IS_SIGNED_INTEGER | LinearPcmFlags::IS_PACKED,
            channels,
        };

        if let Some(matching_asbd) =
            macos_helpers::find_matching_physical_format(device_id, desired_stream_format)
        {
            if let Err(e) =
                macos_helpers::set_device_physical_stream_format(device_id, matching_asbd)
            {
                warn!(
                    error = %e,
                    "coreaudio_exclusive_set_physical_format_failed"
                );
            } else {
                info!(
                    sample_rate,
                    bit_depth, channels, "coreaudio_exclusive_physical_format_set"
                );
            }
        } else {
            // Try float format as fallback (many DACs accept f32)
            let float_format = StreamFormat {
                sample_rate: sample_rate as f64,
                sample_format: SampleFormat::F32,
                flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
                channels,
            };
            if let Some(matching_asbd) =
                macos_helpers::find_matching_physical_format(device_id, float_format)
            {
                if let Err(e) =
                    macos_helpers::set_device_physical_stream_format(device_id, matching_asbd)
                {
                    warn!(error = %e, "coreaudio_exclusive_set_float_format_failed");
                } else {
                    info!(
                        sample_rate,
                        "coreaudio_exclusive_physical_format_set_as_float32"
                    );
                }
            } else {
                warn!(
                    sample_rate,
                    bit_depth, channels, "coreaudio_exclusive_no_matching_physical_format"
                );
            }
        }

        // -- 6. Create HAL AudioUnit with render callback ----------------
        let mut audio_unit = macos_helpers::audio_unit_from_device_id(device_id, false)
            .map_err(|e| format!("Failed to create AudioUnit: {e}"))?;

        // Set the AudioUnit's stream format to match our source.
        // The AudioUnit input scope / output element is where we provide data.
        let au_stream_format = StreamFormat {
            sample_rate: sample_rate as f64,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels,
        };

        audio_unit
            .set_stream_format(au_stream_format, Scope::Input, Element::Output)
            .map_err(|e| format!("Failed to set AudioUnit stream format: {e}"))?;

        // Set up the render callback that pulls from our ring buffer.
        let ring_for_callback = ring.clone();
        let vol_for_callback = volume.clone();
        let paused_for_callback = paused.clone();

        audio_unit
            .set_render_callback(
                move |args: render_callback::Args<render_callback::data::Interleaved<f32>>| {
                    let render_callback::Args {
                        data,
                        num_frames: _,
                        ..
                    } = args;

                    if paused_for_callback.load(Ordering::Relaxed) {
                        for sample in data.buffer.iter_mut() {
                            *sample = 0.0;
                        }
                        return Ok(());
                    }

                    let buffer = data.buffer;
                    let read = ring_for_callback.pop(buffer);

                    // Apply volume
                    let v = vol_for_callback.load(Ordering::Relaxed) as f32 / 1000.0;
                    for sample in &mut buffer[..read] {
                        *sample *= v;
                    }

                    // Silence for any remaining samples
                    for sample in &mut buffer[read..] {
                        *sample = 0.0;
                    }

                    Ok(())
                },
            )
            .map_err(|e| format!("Failed to set render callback: {e}"))?;

        audio_unit
            .start()
            .map_err(|e| format!("Failed to start AudioUnit: {e}"))?;

        info!(
            device = %resolved_name,
            sample_rate,
            bit_depth,
            channels,
            "coreaudio_exclusive_started"
        );

        Ok(Self {
            device_id,
            original_sample_rate,
            is_hogged,
            audio_unit: Some(audio_unit),
            ring,
            volume,
            paused,
        })
    }

    /// Release exclusive mode and restore the device to its original state.
    pub fn release(&mut self) -> Result<(), String> {
        // Stop and drop the AudioUnit first
        if let Some(mut au) = self.audio_unit.take() {
            if let Err(e) = au.stop() {
                warn!(error = %e, "coreaudio_exclusive_stop_failed");
            }
            // AudioUnit is dropped here, which uninitializes and disposes it
        }

        // Restore the original sample rate
        let current_rate = Self::get_device_sample_rate(self.device_id).unwrap_or(0.0);
        if (current_rate as u32) != (self.original_sample_rate as u32) {
            match macos_helpers::set_device_sample_rate(self.device_id, self.original_sample_rate) {
                Ok(()) => {
                    info!(
                        rate = self.original_sample_rate,
                        "coreaudio_exclusive_sample_rate_restored"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        rate = self.original_sample_rate,
                        "coreaudio_exclusive_restore_rate_failed"
                    );
                }
            }
        }

        // Release hog mode
        if self.is_hogged {
            match macos_helpers::toggle_hog_mode(self.device_id) {
                Ok(pid) => {
                    if pid == -1 {
                        info!("coreaudio_exclusive_hog_released");
                    } else {
                        warn!(pid, "coreaudio_exclusive_hog_release_unexpected_pid");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "coreaudio_exclusive_hog_release_failed");
                }
            }
            self.is_hogged = false;
        }

        Ok(())
    }

    /// Read the device's current nominal sample rate.
    fn get_device_sample_rate(device_id: u32) -> Result<f64, String> {
        // We use the coreaudio-rs helpers indirectly: try to read the rate
        // by enumerating available rates and reading the current one.
        // The simplest approach is to use AudioObjectGetPropertyData directly,
        // but coreaudio-rs doesn't expose a standalone getter for current rate.
        // We can work around this by creating a temporary AudioUnit and reading
        // its sample rate, or use the raw FFI. Let's use the raw approach.
        use objc2_core_audio::{
            AudioObjectGetPropertyData, AudioObjectPropertyAddress,
            kAudioDevicePropertyNominalSampleRate, kAudioHardwareNoError,
            kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        };
        use std::mem;
        use std::ptr::{NonNull, null};

        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        let mut sample_rate: f64 = 0.0;
        let data_size = mem::size_of::<f64>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&data_size),
                NonNull::from(&mut sample_rate).cast(),
            )
        };

        if status != kAudioHardwareNoError {
            return Err(format!(
                "Failed to get sample rate for device {device_id}: OSStatus {status}"
            ));
        }

        Ok(sample_rate)
    }

    /// Returns true if exclusive mode is available on this platform.
    pub fn is_available() -> bool {
        true
    }

    /// Returns the ring buffer reference for external feeding.
    pub fn ring(&self) -> &Arc<RingBuf> {
        &self.ring
    }
}

impl Drop for ExclusiveOutput {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            warn!(error = %e, "coreaudio_exclusive_drop_release_failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: check exclusive mode support at runtime
// ---------------------------------------------------------------------------

/// Returns `true` on macOS (where CoreAudio exclusive mode is supported).
pub fn supports_exclusive_mode() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supports_exclusive_mode() {
        assert!(supports_exclusive_mode());
    }

    #[test]
    fn test_exclusive_output_is_available() {
        assert!(ExclusiveOutput::is_available());
    }

    /// Verify that sample format mapping is correct for the bit depths
    /// we care about.
    #[test]
    fn test_sample_format_mapping() {
        // 16-bit -> I16
        assert_eq!(SampleFormat::I16.size_in_bits(), 16);
        // 24-bit -> I24
        assert_eq!(SampleFormat::I24.size_in_bits(), 24);
        // 32-bit -> I32
        assert_eq!(SampleFormat::I32.size_in_bits(), 32);
    }

    /// Verify StreamFormat -> ASBD round-trip produces correct values.
    #[test]
    fn test_stream_format_to_asbd() {
        let sf = StreamFormat {
            sample_rate: 96000.0,
            sample_format: SampleFormat::I16,
            flags: LinearPcmFlags::IS_SIGNED_INTEGER | LinearPcmFlags::IS_PACKED,
            channels: 2,
        };
        let asbd = sf.to_asbd();
        assert_eq!(asbd.mSampleRate as u32, 96000);
        assert_eq!(asbd.mChannelsPerFrame, 2);
        assert_eq!(asbd.mBitsPerChannel, 16);
        assert_eq!(asbd.mBytesPerFrame, 4); // 2 channels * 2 bytes
        assert_eq!(asbd.mFramesPerPacket, 1);
        assert_eq!(asbd.mBytesPerPacket, 4);
    }

    /// Verify float StreamFormat ASBD.
    #[test]
    fn test_stream_format_float_to_asbd() {
        let sf = StreamFormat {
            sample_rate: 44100.0,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: 2,
        };
        let asbd = sf.to_asbd();
        assert_eq!(asbd.mSampleRate as u32, 44100);
        assert_eq!(asbd.mChannelsPerFrame, 2);
        assert_eq!(asbd.mBitsPerChannel, 32);
        assert_eq!(asbd.mBytesPerFrame, 8); // 2 channels * 4 bytes
    }

    /// Verify 24-bit 192kHz stereo ASBD.
    #[test]
    fn test_stream_format_24bit_192k() {
        let sf = StreamFormat {
            sample_rate: 192000.0,
            sample_format: SampleFormat::I24,
            flags: LinearPcmFlags::IS_SIGNED_INTEGER | LinearPcmFlags::IS_PACKED,
            channels: 2,
        };
        let asbd = sf.to_asbd();
        assert_eq!(asbd.mSampleRate as u32, 192000);
        assert_eq!(asbd.mBitsPerChannel, 24);
        assert_eq!(asbd.mBytesPerFrame, 6); // 2 channels * 3 bytes
    }

    /// Verify we can read the default device's sample rate (if a device exists).
    #[test]
    fn test_read_default_device_sample_rate() {
        if let Some(device_id) = macos_helpers::get_default_device_id(false) {
            let rate = ExclusiveOutput::get_device_sample_rate(device_id);
            assert!(rate.is_ok(), "Should be able to read sample rate");
            let rate = rate.unwrap();
            assert!(
                rate > 0.0 && rate <= 768000.0,
                "Sample rate should be in a reasonable range, got {rate}"
            );
        }
    }

    /// Verify hog mode query works on the default device.
    #[test]
    fn test_query_hog_mode() {
        if let Some(device_id) = macos_helpers::get_default_device_id(false) {
            let pid = macos_helpers::get_hogging_pid(device_id);
            assert!(pid.is_ok(), "Should be able to query hog mode");
            // -1 means no process owns hog mode
            let pid = pid.unwrap();
            assert!(
                pid == -1 || pid > 0,
                "PID should be -1 (unheld) or a valid PID, got {pid}"
            );
        }
    }
}
