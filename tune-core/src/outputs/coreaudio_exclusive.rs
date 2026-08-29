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
//!
//! ⚠️ **Ces garanties sont FAIL-CLOSED** (#2235, JP Robbe). Elles échouaient
//! toutes en fail-open : périphérique absent → repli silencieux sur la sortie
//! système, hog refusé → simple warning, cadence refusée → conversion interne
//! acceptée — et le chemin du signal annonçait bit-perfect quand même. Le
//! verdict bit-perfect étant calculé statiquement (`zones.rs`), il ne peut pas
//! connaître l'état d'exécution : la SEULE façon de le rendre vrai est que
//! l'ouverture n'aboutisse que si toutes les garanties tiennent. Si `new()`
//! rend `Ok`, le périphérique demandé est hoggé, à la cadence demandée, au
//! format demandé — sinon c'est une `Err` qui dit quoi faire. Même doctrine
//! que le WASAPI fail-closed de #2233/#2379.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers;
use coreaudio::audio_unit::render_callback;
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope, StreamFormat};
use objc2_core_audio_types::{AudioStreamBasicDescription, kAudioFormatLinearPCM};
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
    original_physical_format: AudioStreamBasicDescription,
    is_hogged: bool,
    format_info: ExclusiveFormatInfo,
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
///
/// Elle decrit l'etat REEL, pas l'intention. Le module s'annoncait
/// « exclusive/bit-perfect » alors que presque toutes ses garanties
/// echouaient en fail-open : peripherique de repli silencieux, hog mode
/// refuse mais poursuivi, cadence materielle refusee mais conversion interne
/// acceptee (JP Robbe, #2235). Le chemin du signal affichait alors
/// bit-perfect a tort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveFormatInfo {
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub device_name: String,
}

#[derive(Debug, Clone, Copy)]
struct RequestedPhysicalFormat {
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
}

trait ExclusiveSetupHal {
    fn resolve_output_device(&mut self, requested_name: &str) -> Result<(u32, String), String>;
    fn nominal_sample_rate(&mut self, device_id: u32) -> Result<f64, String>;
    fn physical_format(&mut self, device_id: u32) -> Result<AudioStreamBasicDescription, String>;
    fn hogging_pid(&mut self, device_id: u32) -> Result<i32, String>;
    fn toggle_hog(&mut self, device_id: u32) -> Result<i32, String>;
    fn set_nominal_sample_rate(&mut self, device_id: u32, rate: f64) -> Result<(), String>;
    fn matching_integer_format(
        &mut self,
        device_id: u32,
        requested: RequestedPhysicalFormat,
    ) -> Result<Option<AudioStreamBasicDescription>, String>;
    fn set_physical_format(
        &mut self,
        device_id: u32,
        format: AudioStreamBasicDescription,
    ) -> Result<(), String>;
}

struct SystemCoreAudioHal;

impl ExclusiveSetupHal for SystemCoreAudioHal {
    fn resolve_output_device(&mut self, requested_name: &str) -> Result<(u32, String), String> {
        let device_id = if requested_name == "default" {
            macos_helpers::get_default_device_id(false)
                .ok_or_else(|| "aucune sortie systeme par defaut disponible".to_string())?
        } else {
            macos_helpers::get_device_id_from_name(requested_name, false).ok_or_else(|| {
                format!(
                    "peripherique introuvable en mode exclusif : {requested_name} — aucun repli silencieux ; choisissez « default » pour viser la sortie systeme"
                )
            })?
        };
        let resolved_name = macos_helpers::get_device_name(device_id)
            .unwrap_or_else(|_| requested_name.to_string());
        Ok((device_id, resolved_name))
    }

    fn nominal_sample_rate(&mut self, device_id: u32) -> Result<f64, String> {
        ExclusiveOutput::get_device_sample_rate(device_id)
    }

    fn physical_format(&mut self, device_id: u32) -> Result<AudioStreamBasicDescription, String> {
        ExclusiveOutput::get_device_physical_format(device_id)
    }

    fn hogging_pid(&mut self, device_id: u32) -> Result<i32, String> {
        macos_helpers::get_hogging_pid(device_id)
            .map_err(|error| format!("lecture du hog mode impossible : {error}"))
    }

    fn toggle_hog(&mut self, device_id: u32) -> Result<i32, String> {
        macos_helpers::toggle_hog_mode(device_id)
            .map_err(|error| format!("bascule du hog mode impossible : {error}"))
    }

    fn set_nominal_sample_rate(&mut self, device_id: u32, rate: f64) -> Result<(), String> {
        macos_helpers::set_device_sample_rate(device_id, rate)
            .map_err(|error| format!("cadence materielle refusee : {error}"))
    }

    fn matching_integer_format(
        &mut self,
        device_id: u32,
        requested: RequestedPhysicalFormat,
    ) -> Result<Option<AudioStreamBasicDescription>, String> {
        let formats = macos_helpers::get_supported_physical_stream_formats(device_id)
            .map_err(|error| format!("enumeration des formats physiques impossible : {error}"))?;
        Ok(formats.into_iter().find_map(|ranged| {
            let mut format = ranged.mFormat;
            let flags = LinearPcmFlags::from_bits_truncate(format.mFormatFlags);
            let rate = requested.sample_rate as f64;
            let exact = format.mFormatID == kAudioFormatLinearPCM
                && flags.contains(LinearPcmFlags::IS_SIGNED_INTEGER)
                && !flags.contains(LinearPcmFlags::IS_FLOAT)
                && format.mBitsPerChannel == requested.bit_depth
                && format.mChannelsPerFrame == requested.channels
                && rate >= ranged.mSampleRateRange.mMinimum
                && rate <= ranged.mSampleRateRange.mMaximum;
            if exact {
                // Les descriptions de plages peuvent porter 0 ou une borne
                // dans mSampleRate. Le contrat demandé doit être écrit, puis
                // relu, à la cadence exacte.
                format.mSampleRate = rate;
                Some(format)
            } else {
                None
            }
        }))
    }

    fn set_physical_format(
        &mut self,
        device_id: u32,
        format: AudioStreamBasicDescription,
    ) -> Result<(), String> {
        macos_helpers::set_device_physical_stream_format(device_id, format)
            .map_err(|error| format!("format physique refuse : {error}"))
    }
}

#[derive(Debug)]
struct PreparedExclusiveDevice {
    device_id: u32,
    device_name: String,
    original_sample_rate: f64,
    original_physical_format: AudioStreamBasicDescription,
    verified_physical_format: AudioStreamBasicDescription,
    is_hogged: bool,
    format_info: ExclusiveFormatInfo,
}

fn physical_layouts_are_equal(
    left: &AudioStreamBasicDescription,
    right: &AudioStreamBasicDescription,
) -> bool {
    (left.mSampleRate - right.mSampleRate).abs() < 0.5
        && left.mFormatID == right.mFormatID
        && left.mFormatFlags == right.mFormatFlags
        && left.mBytesPerPacket == right.mBytesPerPacket
        && left.mFramesPerPacket == right.mFramesPerPacket
        && left.mBytesPerFrame == right.mBytesPerFrame
        && left.mChannelsPerFrame == right.mChannelsPerFrame
        && left.mBitsPerChannel == right.mBitsPerChannel
}

fn validate_physical_format_contract(
    requested: RequestedPhysicalFormat,
    selected: &AudioStreamBasicDescription,
    observed_rate: f64,
    observed: &AudioStreamBasicDescription,
    device_name: &str,
) -> Result<ExclusiveFormatInfo, String> {
    if (observed_rate - requested.sample_rate as f64).abs() >= 0.5 {
        return Err(format!(
            "{device_name} reste a {observed_rate:.0} Hz apres demande de {} Hz",
            requested.sample_rate
        ));
    }

    let selected_flags = LinearPcmFlags::from_bits_truncate(selected.mFormatFlags);
    if selected.mFormatID != kAudioFormatLinearPCM
        || !selected_flags.contains(LinearPcmFlags::IS_SIGNED_INTEGER)
        || selected_flags.contains(LinearPcmFlags::IS_FLOAT)
    {
        return Err(format!(
            "{device_name} ne propose pas le transport PCM entier exige en mode exclusif"
        ));
    }
    if (selected.mSampleRate - requested.sample_rate as f64).abs() >= 0.5
        || selected.mChannelsPerFrame != requested.channels
        || selected.mBitsPerChannel != requested.bit_depth
    {
        return Err(format!(
            "format physique selectionne incompatible sur {device_name}: {} Hz, {} canaux, {} bits",
            selected.mSampleRate, selected.mChannelsPerFrame, selected.mBitsPerChannel
        ));
    }

    if !physical_layouts_are_equal(selected, observed) {
        return Err(format!(
            "read-back physicalFormat different sur {device_name}: attendu {} Hz/{} canaux/{} bits/{} octets par trame/flags {:#x}, observe {} Hz/{} canaux/{} bits/{} octets par trame/flags {:#x}",
            selected.mSampleRate,
            selected.mChannelsPerFrame,
            selected.mBitsPerChannel,
            selected.mBytesPerFrame,
            selected.mFormatFlags,
            observed.mSampleRate,
            observed.mChannelsPerFrame,
            observed.mBitsPerChannel,
            observed.mBytesPerFrame,
            observed.mFormatFlags,
        ));
    }

    if observed.mChannelsPerFrame == 0
        || observed.mBytesPerFrame % observed.mChannelsPerFrame != 0
        || observed.mBytesPerFrame * 8 / observed.mChannelsPerFrame < requested.bit_depth
        || observed.mFramesPerPacket != 1
        || observed.mBytesPerPacket != observed.mBytesPerFrame
    {
        return Err(format!(
            "conteneur physique incoherent sur {device_name}: {} octets par trame pour {} canaux et {} bits valides",
            observed.mBytesPerFrame, observed.mChannelsPerFrame, observed.mBitsPerChannel
        ));
    }

    Ok(ExclusiveFormatInfo {
        sample_rate: observed.mSampleRate as u32,
        bit_depth: observed.mBitsPerChannel,
        channels: observed.mChannelsPerFrame,
        device_name: device_name.to_string(),
    })
}

fn rollback_exclusive_setup<H: ExclusiveSetupHal>(hal: &mut H, prepared: &PreparedExclusiveDevice) {
    if let Err(error) =
        hal.set_physical_format(prepared.device_id, prepared.original_physical_format)
    {
        warn!(%error, "coreaudio_exclusive_rollback_physical_format_failed");
    }
    if let Err(error) =
        hal.set_nominal_sample_rate(prepared.device_id, prepared.original_sample_rate)
    {
        warn!(%error, "coreaudio_exclusive_rollback_sample_rate_failed");
    }
    if prepared.is_hogged {
        if let Err(error) = hal.toggle_hog(prepared.device_id) {
            warn!(%error, "coreaudio_exclusive_rollback_hog_failed");
        }
    }
}

fn prepare_exclusive_device<H: ExclusiveSetupHal>(
    hal: &mut H,
    requested_name: &str,
    requested: RequestedPhysicalFormat,
) -> Result<PreparedExclusiveDevice, String> {
    let (device_id, device_name) = hal.resolve_output_device(requested_name)?;
    let original_sample_rate = hal.nominal_sample_rate(device_id)?;
    let original_physical_format = hal.physical_format(device_id)?;
    let mut prepared = PreparedExclusiveDevice {
        device_id,
        device_name,
        original_sample_rate,
        original_physical_format,
        verified_physical_format: original_physical_format,
        is_hogged: false,
        format_info: ExclusiveFormatInfo {
            sample_rate: 0,
            bit_depth: 0,
            channels: 0,
            device_name: String::new(),
        },
    };

    let my_pid = std::process::id() as i32;
    let current_hog_pid = hal.hogging_pid(device_id)?;
    if current_hog_pid != -1 && current_hog_pid != my_pid {
        return Err(format!(
            "{} est deja reserve par le PID {current_hog_pid}",
            prepared.device_name
        ));
    }
    if current_hog_pid != my_pid {
        let reported_pid = hal.toggle_hog(device_id)?;
        if reported_pid != my_pid {
            return Err(format!(
                "hog mode refuse sur {} : PID observe {reported_pid}, PID attendu {my_pid}",
                prepared.device_name
            ));
        }
    }
    prepared.is_hogged = true;

    if (original_sample_rate - requested.sample_rate as f64).abs() >= 0.5 {
        if let Err(error) = hal.set_nominal_sample_rate(device_id, requested.sample_rate as f64) {
            rollback_exclusive_setup(hal, &prepared);
            return Err(format!(
                "{} refuse {} Hz : {error}",
                prepared.device_name, requested.sample_rate
            ));
        }
    }

    let selected = match hal.matching_integer_format(device_id, requested) {
        Ok(Some(selected)) => selected,
        Ok(None) => {
            rollback_exclusive_setup(hal, &prepared);
            return Err(format!(
                "{} ne propose aucun physicalFormat PCM entier exact a {} Hz, {} canaux et {} bits ; le repli f32 est refuse en mode exclusif",
                prepared.device_name,
                requested.sample_rate,
                requested.channels,
                requested.bit_depth
            ));
        }
        Err(error) => {
            rollback_exclusive_setup(hal, &prepared);
            return Err(format!("{} : {error}", prepared.device_name));
        }
    };
    if let Err(error) = validate_physical_format_contract(
        requested,
        &selected,
        requested.sample_rate as f64,
        &selected,
        &prepared.device_name,
    ) {
        rollback_exclusive_setup(hal, &prepared);
        return Err(error);
    }
    if let Err(error) = hal.set_physical_format(device_id, selected) {
        rollback_exclusive_setup(hal, &prepared);
        return Err(format!("{} : {error}", prepared.device_name));
    }

    let observed_rate = match hal.nominal_sample_rate(device_id) {
        Ok(rate) => rate,
        Err(error) => {
            rollback_exclusive_setup(hal, &prepared);
            return Err(format!(
                "read-back de cadence impossible sur {} : {error}",
                prepared.device_name
            ));
        }
    };
    let observed = match hal.physical_format(device_id) {
        Ok(format) => format,
        Err(error) => {
            rollback_exclusive_setup(hal, &prepared);
            return Err(format!(
                "read-back physicalFormat impossible sur {} : {error}",
                prepared.device_name
            ));
        }
    };
    prepared.format_info = match validate_physical_format_contract(
        requested,
        &selected,
        observed_rate,
        &observed,
        &prepared.device_name,
    ) {
        Ok(info) => info,
        Err(error) => {
            rollback_exclusive_setup(hal, &prepared);
            return Err(error);
        }
    };
    prepared.verified_physical_format = observed;

    Ok(prepared)
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
        let requested = RequestedPhysicalFormat {
            sample_rate,
            bit_depth,
            channels,
        };
        let mut hal = SystemCoreAudioHal;
        let prepared = prepare_exclusive_device(&mut hal, device_name, requested)?;
        let device_id = prepared.device_id;
        let resolved_name = &prepared.device_name;

        info!(
            device = %resolved_name,
            device_id,
            sample_rate,
            bit_depth,
            channels,
            "coreaudio_exclusive_opening"
        );
        let observed = prepared.verified_physical_format;
        let container_bits = observed
            .mBytesPerFrame
            .checked_mul(8)
            .and_then(|bits| bits.checked_div(observed.mChannelsPerFrame))
            .unwrap_or(0);
        info!(
            device = %resolved_name,
            observed_sample_rate = prepared.format_info.sample_rate,
            observed_bit_depth = prepared.format_info.bit_depth,
            observed_channels = prepared.format_info.channels,
            observed_container_bits = container_bits,
            observed_format_flags = observed.mFormatFlags,
            "coreaudio_exclusive_physical_contract_verified"
        );

        // -- 6. Create HAL AudioUnit with render callback ----------------
        let mut audio_unit = match macos_helpers::audio_unit_from_device_id(device_id, false) {
            Ok(audio_unit) => audio_unit,
            Err(error) => {
                rollback_exclusive_setup(&mut hal, &prepared);
                return Err(format!("Failed to create AudioUnit: {error}"));
            }
        };

        // Set the AudioUnit's stream format to match our source.
        // The AudioUnit input scope / output element is where we provide data.
        let au_stream_format = StreamFormat {
            sample_rate: sample_rate as f64,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels,
        };

        if let Err(error) =
            audio_unit.set_stream_format(au_stream_format, Scope::Input, Element::Output)
        {
            rollback_exclusive_setup(&mut hal, &prepared);
            return Err(format!("Failed to set AudioUnit stream format: {error}"));
        }

        // Set up the render callback that pulls from our ring buffer.
        let ring_for_callback = ring.clone();
        let vol_for_callback = volume.clone();
        let paused_for_callback = paused.clone();

        if let Err(error) = audio_unit.set_render_callback(
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
        ) {
            rollback_exclusive_setup(&mut hal, &prepared);
            return Err(format!("Failed to set render callback: {error}"));
        }

        if let Err(error) = audio_unit.start() {
            rollback_exclusive_setup(&mut hal, &prepared);
            return Err(format!("Failed to start AudioUnit: {error}"));
        }

        info!(
            device = %resolved_name,
            sample_rate = prepared.format_info.sample_rate,
            bit_depth = prepared.format_info.bit_depth,
            channels = prepared.format_info.channels,
            "coreaudio_exclusive_started"
        );

        Ok(Self {
            device_id,
            original_sample_rate: prepared.original_sample_rate,
            original_physical_format: prepared.original_physical_format,
            is_hogged: prepared.is_hogged,
            format_info: prepared.format_info,
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

        // Le format physique appartient aussi au bail exclusif. Le laisser
        // derrière nous pouvait modifier durablement le comportement du DAC
        // après un échec ou la fin de lecture.
        match Self::get_device_physical_format(self.device_id) {
            Ok(current)
                if !physical_layouts_are_equal(&current, &self.original_physical_format) =>
            {
                match macos_helpers::set_device_physical_stream_format(
                    self.device_id,
                    self.original_physical_format,
                ) {
                    Ok(()) => info!("coreaudio_exclusive_physical_format_restored"),
                    Err(error) => warn!(
                        %error,
                        "coreaudio_exclusive_restore_physical_format_failed"
                    ),
                }
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "coreaudio_exclusive_read_physical_format_failed"),
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

    /// Read the format in which the device stream actually performs I/O.
    fn get_device_physical_format(device_id: u32) -> Result<AudioStreamBasicDescription, String> {
        use objc2_core_audio::{
            AudioObjectGetPropertyData, AudioObjectPropertyAddress, kAudioHardwareNoError,
            kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
            kAudioStreamPropertyPhysicalFormat,
        };
        use std::mem::{self, MaybeUninit};
        use std::ptr::{NonNull, null};

        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioStreamPropertyPhysicalFormat,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut physical_format = MaybeUninit::<AudioStreamBasicDescription>::zeroed();
        let mut data_size = mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::from(&mut physical_format).cast(),
            )
        };
        if status != kAudioHardwareNoError {
            return Err(format!(
                "lecture du physicalFormat impossible pour le peripherique {device_id} : OSStatus {status}"
            ));
        }

        Ok(unsafe { physical_format.assume_init() })
    }

    /// Returns true if exclusive mode is available on this platform.
    pub fn is_available() -> bool {
        true
    }

    /// Returns the ring buffer reference for external feeding.
    pub fn ring(&self) -> &Arc<RingBuf> {
        &self.ring
    }

    /// Contract observed after CoreAudio accepted the requested setup.
    pub fn format_info(&self) -> &ExclusiveFormatInfo {
        &self.format_info
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

    fn requested_24_96() -> RequestedPhysicalFormat {
        RequestedPhysicalFormat {
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
        }
    }

    fn integer_asbd(
        sample_rate: u32,
        bit_depth: u32,
        channels: u32,
    ) -> AudioStreamBasicDescription {
        let sample_format = match bit_depth {
            16 => SampleFormat::I16,
            24 => SampleFormat::I24,
            32 => SampleFormat::I32,
            _ => panic!("profondeur de test non prise en charge"),
        };
        StreamFormat {
            sample_rate: sample_rate as f64,
            sample_format,
            flags: LinearPcmFlags::IS_SIGNED_INTEGER | LinearPcmFlags::IS_PACKED,
            channels,
        }
        .to_asbd()
    }

    #[derive(Debug)]
    struct MockHal {
        device_present: bool,
        original_rate: f64,
        observed_rate: f64,
        original_format: AudioStreamBasicDescription,
        observed_format: AudioStreamBasicDescription,
        hog_pid: i32,
        toggle_pid: i32,
        rate_refused: bool,
        matching_format: Option<AudioStreamBasicDescription>,
        physical_set_refused: bool,
        nominal_reads: usize,
        physical_reads: usize,
        events: Vec<&'static str>,
        resolved_names: Vec<String>,
    }

    impl MockHal {
        fn exact() -> Self {
            let selected = integer_asbd(96000, 24, 2);
            Self {
                device_present: true,
                original_rate: 48000.0,
                observed_rate: 96000.0,
                original_format: integer_asbd(48000, 16, 2),
                observed_format: selected,
                hog_pid: -1,
                toggle_pid: std::process::id() as i32,
                rate_refused: false,
                matching_format: Some(selected),
                physical_set_refused: false,
                nominal_reads: 0,
                physical_reads: 0,
                events: Vec::new(),
                resolved_names: Vec::new(),
            }
        }
    }

    impl ExclusiveSetupHal for MockHal {
        fn resolve_output_device(&mut self, requested_name: &str) -> Result<(u32, String), String> {
            self.events.push("resolve");
            self.resolved_names.push(requested_name.to_string());
            if self.device_present {
                Ok((42, requested_name.to_string()))
            } else {
                Err(format!("peripherique introuvable : {requested_name}"))
            }
        }

        fn nominal_sample_rate(&mut self, _device_id: u32) -> Result<f64, String> {
            self.events.push("read_rate");
            let rate = if self.nominal_reads == 0 {
                self.original_rate
            } else {
                self.observed_rate
            };
            self.nominal_reads += 1;
            Ok(rate)
        }

        fn physical_format(
            &mut self,
            _device_id: u32,
        ) -> Result<AudioStreamBasicDescription, String> {
            self.events.push("read_physical");
            let format = if self.physical_reads == 0 {
                self.original_format
            } else {
                self.observed_format
            };
            self.physical_reads += 1;
            Ok(format)
        }

        fn hogging_pid(&mut self, _device_id: u32) -> Result<i32, String> {
            self.events.push("read_hog");
            Ok(self.hog_pid)
        }

        fn toggle_hog(&mut self, _device_id: u32) -> Result<i32, String> {
            self.events.push("toggle_hog");
            Ok(self.toggle_pid)
        }

        fn set_nominal_sample_rate(&mut self, _device_id: u32, _rate: f64) -> Result<(), String> {
            self.events.push("set_rate");
            if self.rate_refused {
                Err("cadence refusee".to_string())
            } else {
                Ok(())
            }
        }

        fn matching_integer_format(
            &mut self,
            _device_id: u32,
            _requested: RequestedPhysicalFormat,
        ) -> Result<Option<AudioStreamBasicDescription>, String> {
            self.events.push("find_integer");
            Ok(self.matching_format)
        }

        fn set_physical_format(
            &mut self,
            _device_id: u32,
            _format: AudioStreamBasicDescription,
        ) -> Result<(), String> {
            self.events.push("set_physical");
            if self.physical_set_refused {
                Err("format refuse".to_string())
            } else {
                Ok(())
            }
        }
    }

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

    #[test]
    fn physical_contract_rejects_a_float_readback() {
        let requested = RequestedPhysicalFormat {
            sample_rate: 192000,
            bit_depth: 24,
            channels: 2,
        };
        let selected = StreamFormat {
            sample_rate: 192000.0,
            sample_format: SampleFormat::I24,
            flags: LinearPcmFlags::IS_SIGNED_INTEGER | LinearPcmFlags::IS_PACKED,
            channels: 2,
        }
        .to_asbd();
        let observed = StreamFormat {
            sample_rate: 192000.0,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: 2,
        }
        .to_asbd();

        let result =
            validate_physical_format_contract(requested, &selected, 192000.0, &observed, "DAC");

        assert!(
            result.is_err(),
            "un read-back f32 doit invalider le contrat"
        );
    }

    #[test]
    fn exclusive_setup_never_substitutes_a_missing_named_device() {
        let mut hal = MockHal::exact();
        hal.device_present = false;

        let error = prepare_exclusive_device(&mut hal, "DAC USB", requested_24_96())
            .expect_err("un peripherique nomme absent doit arreter la preparation");

        assert!(error.contains("DAC USB"));
        assert_eq!(hal.resolved_names, vec!["DAC USB"]);
        assert_eq!(hal.events, vec!["resolve"]);
    }

    #[test]
    fn exclusive_setup_fails_when_hog_is_refused() {
        let mut hal = MockHal::exact();
        hal.toggle_pid = 777;

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("un hog attribue a un autre PID doit etre refuse");

        assert!(error.contains("hog mode refuse"));
        assert!(!hal.events.contains(&"set_rate"));
        assert!(!hal.events.contains(&"set_physical"));
    }

    #[test]
    fn exclusive_setup_rolls_back_hog_when_rate_is_refused() {
        let mut hal = MockHal::exact();
        hal.rate_refused = true;

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("une cadence refusee doit arreter la preparation");

        assert!(error.contains("refuse 96000 Hz"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            2,
            "le second toggle rend le hog acquis pendant la tentative"
        );
    }

    #[test]
    fn exclusive_setup_rejects_float_only_instead_of_falling_back() {
        let mut hal = MockHal::exact();
        hal.matching_format = None;

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("un DAC sans format entier exact ne doit pas passer en f32");

        assert!(error.contains("repli f32 est refuse"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            2
        );
    }

    #[test]
    fn exclusive_setup_rejects_a_refused_physical_format_and_rolls_back() {
        let mut hal = MockHal::exact();
        hal.physical_set_refused = true;

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("un format physique refuse doit arreter la preparation");

        assert!(error.contains("format refuse"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            2
        );
    }

    #[test]
    fn exclusive_setup_rejects_a_different_nominal_rate_readback() {
        let mut hal = MockHal::exact();
        hal.observed_rate = 48000.0;

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("une cadence relue differente doit invalider le contrat");

        assert!(error.contains("reste a 48000 Hz"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            2
        );
    }

    #[test]
    fn exclusive_setup_requires_the_exact_channel_layout() {
        let mut hal = MockHal::exact();
        hal.matching_format = Some(integer_asbd(96000, 24, 8));

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("un format huit canaux ne doit pas valider une demande stereo");

        assert!(error.contains("8 canaux"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "set_physical")
                .count(),
            1,
            "seule la restauration peut ecrire : le format incompatible ne doit jamais etre arme"
        );
    }

    #[test]
    fn exclusive_setup_rejects_a_different_physical_readback_and_rolls_back() {
        let mut hal = MockHal::exact();
        hal.observed_format = StreamFormat {
            sample_rate: 96000.0,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: 2,
        }
        .to_asbd();

        let error = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect_err("le read-back different doit invalider le contrat");

        assert!(error.contains("read-back physicalFormat different"));
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "set_physical")
                .count(),
            2,
            "la seconde ecriture restaure le format physique original"
        );
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            2
        );
    }

    #[test]
    fn exclusive_setup_returns_only_an_observed_exact_contract() {
        let mut hal = MockHal::exact();

        let prepared = prepare_exclusive_device(&mut hal, "DAC", requested_24_96())
            .expect("le contrat exact doit etre accepte");

        assert_eq!(
            prepared.format_info,
            ExclusiveFormatInfo {
                sample_rate: 96000,
                bit_depth: 24,
                channels: 2,
                device_name: "DAC".to_string(),
            }
        );
        assert_eq!(
            hal.events
                .iter()
                .filter(|event| **event == "toggle_hog")
                .count(),
            1,
            "aucun rollback ne doit avoir lieu avant le demarrage AudioUnit"
        );
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

    #[test]
    fn test_read_default_device_physical_format() {
        if let Some(device_id) = macos_helpers::get_default_device_id(false) {
            let format = ExclusiveOutput::get_device_physical_format(device_id)
                .expect("le physicalFormat du peripherique par defaut doit etre lisible");
            assert!(format.mSampleRate > 0.0);
            assert!(format.mChannelsPerFrame > 0);
            assert!(format.mBytesPerFrame > 0);
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
