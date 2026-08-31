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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{debug, info, warn};

use super::local::{NativePcmRing, RingBuf};

#[derive(Default)]
struct RealtimeCounters {
    underruns: AtomicU64,
    callback_errors: AtomicU64,
}

/// Process-wide guard serializing access to the single ASIO device. ASIO
/// forbids two concurrent streams on the same device — even within one
/// process. When a track ends and the user force-plays another, a new stream
/// used to be opened ~1 ms after the previous one was aborted, before the old
/// instance's Drop had released the driver. That race crashed the Fireface
/// ASIO driver natively (no Rust panic, process gone). Holding this lock for
/// the whole session (acquired in `new`, released after Drop tears the driver
/// down and settles) makes the new open WAIT for the old one instead.
static ASIO_DEVICE_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` while holding the process-wide ASIO device lock **only if it is
/// free**. Returns `None` — without running `f` — when the lock is currently
/// held, i.e. an exclusive playback session is active (or still settling in
/// `Drop`).
///
/// This is what enumeration must use. `AsioExclusiveOutput::new` holds
/// [`ASIO_DEVICE_LOCK`] for the whole playback session, but device *listing*
/// (Settings, the Diagnostics page, the `/devices/audio*` API) used to call
/// `supported_output_configs()` on the driver without any lock — opening the
/// single-instance ASIO driver a second time. On drivers like SOtM Diretta or
/// RME Fireface that concurrent open churns the driver so it never finishes
/// locking (observed: endless connect → getBufferSize → disconnect cycles,
/// never reaching `createBuffers`/`start`). Probing through this guard makes
/// enumeration back off to cached data instead of racing an active stream.
pub fn try_with_asio_device_lock<R>(f: impl FnOnce() -> R) -> Option<R> {
    match ASIO_DEVICE_LOCK.try_lock() {
        Ok(_guard) => Some(f()),
        Err(std::sync::TryLockError::WouldBlock) => None,
        // A prior holder panicked; the driver state is still consistent enough
        // to enumerate, so recover the guard and proceed.
        Err(std::sync::TryLockError::Poisoned(p)) => {
            let _guard = p.into_inner();
            Some(f())
        }
    }
}

/// `true` while an exclusive playback session owns the single-instance ASIO
/// driver (including the settle window in `Drop`).
///
/// The side-effect-free companion of [`try_with_asio_device_lock`], for callers
/// that must decide whether they may touch the driver at all — the generic
/// device enumeration in `local.rs` cannot hold the lock around its own probe
/// without also serialising the non-ASIO hosts it may end up opening.
///
/// A poisoned lock reports "free": a prior holder panicked, no stream owns the
/// driver, and refusing to enumerate for the rest of the session would be worse
/// than probing it.
pub fn asio_device_is_busy() -> bool {
    matches!(
        ASIO_DEVICE_LOCK.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}

/// Base time given to the ASIO driver to fully release the hardware after a
/// stream is torn down, before the device lock is released and the next open
/// runs.
const ASIO_TEARDOWN_SETTLE_BASE: Duration = Duration::from_millis(200);

/// Le zéro 24 bits, en constante. `cpal::I24::new(0)` rend un `Option`, et
/// l'écrire dans un rappel temps réel obligeait à un `.expect()` — une
/// panique potentielle sur le fil audio du pilote.
///
/// ⚠️ Forme QUALIFIÉE : `EQUILIBRIUM` vient du trait `Sample`, pas d'une
/// constante inhérente. `cpal::I24::EQUILIBRIUM` ne compile pas ici, le trait
/// n'étant pas importé — et ce fichier est sous
/// `#[cfg(all(target_os = "windows", feature = "asio"))]`, donc AUCUNE porte
/// Linux ne l'aurait dit.
const I24_ZERO: cpal::I24 = <cpal::I24 as cpal::Sample>::EQUILIBRIUM;

/// Conversion TOTALE vers 24 bits : elle ne panique jamais.
///
/// ⚠️ Les cinq `.expect()` qu'elle remplace étaient dans les **rappels
/// temps réel** ASIO. Une panique y est bien pire qu'ailleurs : elle traverse
/// le fil du pilote, et sur les plateformes sans hook de panique (toutes sauf
/// Windows) elle ne laisse même pas de trace. Aucun de ces cinq cas n'était
/// atteignable — `0` tient sur 24 bits, `i32 >> 8` aussi par construction, et
/// la valeur du chemin traité est bornée juste avant — mais un `.expect()`
/// dans un rappel audio est un pari, pas une garantie.
///
/// ⛔ **Ne pas utiliser `I24::from(i32)`** : ce `From` fait un
/// `wrap_overflow()`. Un échantillon hors borne y basculerait en polarité
/// opposée — un claquement à pleine échelle, exactement ce qu'un dépassement
/// ne doit pas produire. On borne, puis on se replie sur le silence.
#[inline]
fn i24_borne(valeur: i32) -> cpal::I24 {
    let borne = valeur.clamp(-8_388_608, 8_388_607);
    cpal::I24::new(borne).unwrap_or(I24_ZERO)
}

/// Settle time before releasing the device lock, scaled with the stream's
/// sample rate. High-rate streams (e.g. 176.4 / 192 kHz) need a longer clock
/// re-lock/release on the driver: with a flat 200 ms, a rapid Repeat-One /
/// next transition on a 176.4 kHz stream reopened the exclusive device before
/// the driver had fully released it, and the new stream got `frames=0` — which
/// forced an expensive full network re-download (DEvir, RME Fireface USB,
/// Repeat One 176.4 kHz). +100 ms per 48 kHz step above 48 kHz, capped at
/// +400 ms (→ 600 ms at 192 kHz).
fn teardown_settle_for(sample_rate: u32) -> Duration {
    if sample_rate <= 48_000 {
        ASIO_TEARDOWN_SETTLE_BASE
    } else {
        let steps = (u64::from(sample_rate) / 48_000).min(4);
        ASIO_TEARDOWN_SETTLE_BASE + Duration::from_millis(steps * 100)
    }
}

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
    #[allow(dead_code)]
    float_ring: Arc<RingBuf>,
    #[allow(dead_code)]
    native_ring: Arc<NativePcmRing>,
    transport: AsioTransport,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    volume: Arc<AtomicU32>,
    /// Kept alive for the render callback closure.
    #[allow(dead_code)]
    paused: Arc<AtomicBool>,
    counters: Arc<RealtimeCounters>,
    /// Held for the whole session so no other ASIO stream can open on the
    /// device concurrently. Released (with a settle delay) in `Drop`.
    #[allow(dead_code)]
    device_guard: MutexGuard<'static, ()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsioTransport {
    NativeI32,
    NativeI24,
    NativeI16,
    ProcessedI32(&'static str),
    ProcessedI24(&'static str),
    ProcessedI16(&'static str),
    ProcessedF32(&'static str),
}

impl AsioTransport {
    fn for_format(native_format: SampleFormat, source_bit_depth: u32) -> Result<Self, String> {
        match native_format {
            SampleFormat::I32 if matches!(source_bit_depth, 16 | 24 | 32) => Ok(Self::NativeI32),
            SampleFormat::I32 => Ok(Self::ProcessedI32(
                "la profondeur source ne possède pas de représentation entière native prise en charge",
            )),
            SampleFormat::I24 if matches!(source_bit_depth, 16 | 24) => Ok(Self::NativeI24),
            SampleFormat::I24 => Ok(Self::ProcessedI24(
                "le callback 24 bits du pilote ASIO ne peut pas conserver tous les bits de la source",
            )),
            SampleFormat::I16 if source_bit_depth == 16 => Ok(Self::NativeI16),
            SampleFormat::I16 => Ok(Self::ProcessedI16(
                "le pilote ASIO n'accepte que des mots 16 bits pour cette configuration",
            )),
            SampleFormat::F32 => Ok(Self::ProcessedF32(
                "le pilote ASIO expose uniquement un callback flottant pour cette configuration",
            )),
            other => Err(format!(
                "Format natif ASIO non pris en charge sans conversion implicite : {other:?}"
            )),
        }
    }

    fn is_native(self) -> bool {
        matches!(self, Self::NativeI32 | Self::NativeI24 | Self::NativeI16)
    }

    fn bit_perfect_unavailable_reason(self) -> Option<&'static str> {
        match self {
            Self::ProcessedI32(reason)
            | Self::ProcessedI24(reason)
            | Self::ProcessedI16(reason)
            | Self::ProcessedF32(reason) => Some(reason),
            Self::NativeI32 | Self::NativeI24 | Self::NativeI16 => None,
        }
    }
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
        float_ring: Arc<RingBuf>,
        native_ring: Arc<NativePcmRing>,
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

        let transport = AsioTransport::for_format(native_fmt, bit_depth)?;
        if let Some(reason) = transport.bit_perfect_unavailable_reason() {
            info!(
                device = %resolved_name,
                source_bit_depth = bit_depth,
                native_format = ?native_fmt,
                reason,
                "asio_bit_perfect_transport_unavailable"
            );
        }

        // -- 5. Build output stream with render callback --------------------
        //
        // Integer-compatible drivers consume the left-aligned native ring
        // directly. Only incompatible configurations retain the processed
        // f32 path, with an explicit reason logged above.
        let counters = Arc::new(RealtimeCounters::default());
        let stream = Self::build_native_stream(
            &device,
            &stream_config,
            transport,
            float_ring.clone(),
            native_ring.clone(),
            volume.clone(),
            paused.clone(),
            counters.clone(),
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
            float_ring,
            native_ring,
            transport,
            volume,
            paused,
            counters,
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

        info!(
            device = %self.device_name,
            underruns = self.underrun_count(),
            callback_errors = self.callback_error_count(),
            "asio_exclusive_released"
        );
        Ok(())
    }

    /// Returns true if ASIO exclusive mode is available on this platform.
    pub fn is_available() -> bool {
        // Check if we can actually get an ASIO host
        cpal::host_from_id(cpal::HostId::Asio).is_ok()
    }

    /// Whether the selected ASIO callback can consume left-aligned integer
    /// words without a floating-point round trip.
    pub fn uses_native_transport(&self) -> bool {
        self.transport.is_native()
    }

    pub fn underrun_count(&self) -> u64 {
        self.counters.underruns.load(Ordering::Relaxed)
    }

    pub fn callback_error_count(&self) -> u64 {
        self.counters.callback_errors.load(Ordering::Relaxed)
    }

    /// Human-readable reason exposed when this device/configuration cannot
    /// honour a bit-perfect integer contract.
    pub fn bit_perfect_unavailable_reason(&self) -> Option<&'static str> {
        self.transport.bit_perfect_unavailable_reason()
    }

    /// Build the cpal output stream using the driver's native sample format.
    ///
    /// Integer callbacks only move native words. The f32 callback is retained
    /// for drivers whose advertised format cannot represent the source words
    /// exactly; its reason is exposed by `bit_perfect_unavailable_reason`.
    fn build_native_stream(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        transport: AsioTransport,
        float_ring: Arc<RingBuf>,
        native_ring: Arc<NativePcmRing>,
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
        counters: Arc<RealtimeCounters>,
    ) -> Result<cpal::Stream, String> {
        match transport {
            AsioTransport::NativeI32 => {
                info!("asio_exclusive_building_i32_stream");
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let read = native_ring.pop(data);
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(0);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO I32 stream: {e}"))
            }
            AsioTransport::NativeI24 => {
                info!("asio_exclusive_building_native_i24_stream");
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [cpal::I24], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(I24_ZERO);
                                return;
                            }
                            let read =
                                native_ring.pop_mapped(data, |sample| i24_borne(sample >> 8));
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(I24_ZERO);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO I24 stream: {e}"))
            }
            AsioTransport::NativeI16 => {
                info!("asio_exclusive_building_i16_stream");
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let read = native_ring.pop_mapped(data, |sample| (sample >> 16) as i16);
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(0);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build ASIO I16 stream: {e}"))
            }
            AsioTransport::ProcessedI32(_) => {
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let volume = volume.load(Ordering::Relaxed) as f64 / 1000.0;
                            let read = float_ring.pop_mapped(data, |sample| {
                                let scaled = (f64::from(sample) * volume * 2_147_483_648.0)
                                    .round()
                                    .clamp(i32::MIN as f64, i32::MAX as f64);
                                scaled as i32
                            });
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(0);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build processed ASIO I32 stream: {e}"))
            }
            AsioTransport::ProcessedI24(_) => {
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [cpal::I24], _: &cpal::OutputCallbackInfo| {
                            let zero = I24_ZERO;
                            if paused.load(Ordering::Relaxed) {
                                data.fill(zero);
                                return;
                            }
                            let volume = volume.load(Ordering::Relaxed) as f64 / 1000.0;
                            let read = float_ring.pop_mapped(data, |sample| {
                                let scaled = (f64::from(sample) * volume * 8_388_608.0)
                                    .round()
                                    .clamp(-8_388_608.0, 8_388_607.0)
                                    as i32;
                                i24_borne(scaled)
                            });
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(zero);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build processed ASIO I24 stream: {e}"))
            }
            AsioTransport::ProcessedI16(_) => {
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let volume = volume.load(Ordering::Relaxed) as f64 / 1000.0;
                            let read = float_ring.pop_mapped(data, |sample| {
                                let scaled = (f64::from(sample) * volume * 32_768.0)
                                    .round()
                                    .clamp(i16::MIN as f64, i16::MAX as f64);
                                scaled as i16
                            });
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            data[read..].fill(0);
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| format!("Failed to build processed ASIO I16 stream: {e}"))
            }
            AsioTransport::ProcessedF32(_) => {
                let errors = counters.clone();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if paused.load(Ordering::Relaxed) {
                                data.fill(0.0);
                                return;
                            }
                            let read = float_ring.pop(data);
                            if read < data.len() {
                                counters.underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            let v = volume.load(Ordering::Relaxed) as f32 / 1000.0;
                            for sample in &mut data[..read] {
                                *sample *= v;
                            }
                            for sample in &mut data[read..] {
                                *sample = 0.0;
                            }
                        },
                        move |_| {
                            errors.callback_errors.fetch_add(1, Ordering::Relaxed);
                        },
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
        // Fireface, crashes the process natively. Scale the wait with the
        // sample rate — high rates need longer to release (fixes frames=0 on
        // 176.4 kHz Repeat-One transitions).
        std::thread::sleep(teardown_settle_for(self.current_sample_rate));
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

    #[test]
    fn native_asio_transport_is_selected_only_when_every_source_bit_fits() {
        assert_eq!(
            AsioTransport::for_format(SampleFormat::I32, 32).unwrap(),
            AsioTransport::NativeI32
        );
        assert_eq!(
            AsioTransport::for_format(SampleFormat::I24, 24).unwrap(),
            AsioTransport::NativeI24
        );
        assert_eq!(
            AsioTransport::for_format(SampleFormat::I16, 16).unwrap(),
            AsioTransport::NativeI16
        );
        assert!(
            AsioTransport::for_format(SampleFormat::I16, 24)
                .unwrap()
                .bit_perfect_unavailable_reason()
                .is_some()
        );
        assert!(
            AsioTransport::for_format(SampleFormat::F32, 24)
                .unwrap()
                .bit_perfect_unavailable_reason()
                .is_some()
        );
    }

    // ── i24_borne : la conversion des rappels temps réel ──────────────────
    //
    // Ces tests remplacent cinq `.expect()` qui vivaient dans les rappels
    // ASIO. Ils ne prouvent pas qu'une valeur hors borne arrive — aucune ne
    // peut arriver — ils prouvent que la conversion est TOTALE si elle
    // arrivait, au lieu de tuer le fil du pilote.

    /// Le témoin : sur tout ce que les rappels produisent réellement, la
    /// conversion rend la valeur EXACTE. Un correctif qui se contenterait de
    /// rendre le silence partout passerait le test de totalité et casserait
    /// l'audio — celui-ci l'attrape.
    #[test]
    fn i24_borne_rend_la_valeur_exacte_sur_le_domaine_reel() {
        for v in [0, 1, -1, 8_388_607, -8_388_608, 4_194_304, -4_194_304] {
            assert_eq!(i24_borne(v).inner(), v, "valeur dans le domaine 24 bits");
        }
        // `sample >> 8`, le mot natif décalé du chemin NativeI24 : il tient
        // par construction, aux deux extrêmes de i32.
        assert_eq!(i24_borne(i32::MAX >> 8).inner(), 8_388_607);
        assert_eq!(i24_borne(i32::MIN >> 8).inner(), -8_388_608);
    }

    /// La totalité : aucune entrée i32 ne peut faire paniquer un rappel.
    #[test]
    fn i24_borne_ne_panique_sur_aucune_valeur_i32() {
        for v in [
            i32::MIN,
            i32::MIN + 1,
            -8_388_609,
            8_388_608,
            i32::MAX - 1,
            i32::MAX,
        ] {
            let sortie = i24_borne(v).inner();
            assert!(
                (-8_388_608..=8_388_607).contains(&sortie),
                "hors borne rendait {sortie} au lieu de rester dans 24 bits"
            );
        }
    }

    /// ⛔ Ce qu'on refuse : `I24::from(i32)` fait un `wrap_overflow()`. Une
    /// valeur au-dessus du maximum y ressort NÉGATIVE — un claquement à pleine
    /// échelle. Ce test fige le contraste : `i24_borne` sature du bon côté.
    #[test]
    fn i24_borne_sature_au_lieu_de_replier_la_polarite() {
        let trop_haut = 8_388_608_i32;
        assert_eq!(
            i24_borne(trop_haut).inner(),
            8_388_607,
            "doit saturer en haut"
        );
        assert!(
            i24_borne(trop_haut).inner() > 0,
            "un dépassement positif ne doit jamais ressortir négatif"
        );
        assert_eq!(
            i24_borne(-8_388_609).inner(),
            -8_388_608,
            "doit saturer en bas"
        );
    }

    /// Le zéro des rappels est bien le silence, pas une valeur arbitraire.
    #[test]
    fn i24_zero_est_le_silence() {
        assert_eq!(I24_ZERO.inner(), 0);
    }
}
