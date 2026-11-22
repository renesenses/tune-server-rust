//! Native rate stage. CREATE uses other for the output rate, and returns
//! latency in other. PROCESS borrows two distinct buffers (data and reply).
//! On return frames counts input consumed and other counts output written.
//! DRAIN borrows only reply and sets flags when the tail is exhausted.
//! The caller retains ownership of both audio buffers throughout the call.
use super::*;
use tune_plugin_sdk::rate::{RateChanger, RateChangerFactory, RateRequest};

pub const KIND: u32 = 3;
pub const NEGOTIATE: u32 = 10;

#[derive(Serialize, Deserialize)]
pub struct Negotiation {
    pub accepted: Vec<u32>,
    pub settings: Settings,
}

struct Stage(Box<dyn RateChanger>);

unsafe fn block<'a>(
    buffer: Buffer,
    format: AudioFormat,
    limit: usize,
) -> Result<AudioBlock<'a>, Error> {
    let wire = Format::from_audio(format);
    let bytes = usize::try_from(buffer.len).map_err(|_| Error::BlockTooLarge)?;
    let stride = wire.sample_bytes()? * usize::from(format.channels());
    if !bytes.is_multiple_of(stride) || bytes / stride > limit || bytes > isize::MAX as usize {
        return Err(Error::InvalidFormat);
    }
    let ptr = if bytes == 0 {
        std::ptr::NonNull::<u64>::dangling().as_ptr().cast()
    } else if buffer.data.is_null() {
        return Err(Error::InvalidFormat);
    } else {
        buffer.data
    };
    AudioBlock::new(
        format,
        unsafe { borrow_samples(ptr, bytes, wire.encoding)? },
        limit,
    )
}

/// # Safety
/// Same handle and ownership requirements as dsp_call. For PROCESS the caller
/// lends disjoint, exclusively accessible, initialized input/output storage.
/// DRAIN lends initialized output storage only. Neither buffer may alias raw.
pub unsafe fn call(factory: &dyn RateChangerFactory, raw: *mut Request) -> i32 {
    boundary(|| {
        if raw.is_null() || unsafe { raw.cast::<u32>().read() } != size_of::<Request>() as u32 {
            return Err(Error::InvalidState);
        }
        let r = unsafe { &mut *raw };
        match r.operation {
            NEGOTIATE => {
                let n: Negotiation = serde_json::from_slice(unsafe { r.data.bytes()? })
                    .map_err(|_| Error::InvalidSettings)?;
                let rate = factory.negotiate(r.format.audio()?, &n.accepted, &n.settings)?;
                if rate.is_some_and(|v| v == 0 || !n.accepted.contains(&v)) {
                    return Err(Error::InvalidFormat);
                }
                r.other = u64::from(rate.unwrap_or(0));
                return Ok(());
            }
            CREATE => {
                let request = RateRequest::new(
                    r.format.audio()?,
                    u32::try_from(r.other).map_err(|_| Error::InvalidFormat)?,
                    r.frames as usize,
                )?;
                let settings = serde_json::from_slice(unsafe { r.data.bytes()? })
                    .map_err(|_| Error::InvalidSettings)?;
                let stage = factory.prepare(request, &settings)?;
                if stage.request() != request {
                    return Err(Error::InvalidState);
                }
                r.other = u64::from(stage.latency_frames());
                r.handle = Box::into_raw(Box::new(Stage(stage))) as usize as u64;
                return Ok(());
            }
            _ if r.handle == 0 => return Err(Error::InvalidState),
            DESTROY => {
                drop(unsafe { Box::from_raw(r.handle as usize as *mut Stage) });
                r.handle = 0;
                return Ok(());
            }
            _ => {}
        }
        let stage = &mut unsafe { &mut *(r.handle as usize as *mut Stage) }.0;
        let request = stage.request();
        match r.operation {
            PROCESS => {
                let input = unsafe { block(r.data, request.input, request.max_input_frames)? };
                let mut output =
                    unsafe { block(r.reply, request.output_format(), stage.max_output_frames())? };
                let report = stage.process(
                    &input,
                    &mut output,
                    BlockContext {
                        zone_id: r.zone_id,
                        generation: r.generation,
                        position_frames: r.position_frames,
                    },
                )?;
                if report.frames_written > output.frames()
                    || report.frames_consumed != input.frames()
                {
                    return Err(Error::InvalidState);
                }
                r.frames = report.frames_consumed as u32;
                r.other = report.frames_written as u64;
                r.clipped = report.clipped_samples;
                r.non_finite = report.non_finite_samples;
            }
            DRAIN => {
                let mut output =
                    unsafe { block(r.reply, request.output_format(), stage.max_output_frames())? };
                let report = stage.drain(&mut output)?;
                if report.frames_written > output.frames() {
                    return Err(Error::InvalidState);
                }
                r.other = report.frames_written as u64;
                r.flags = u32::from(report.complete);
            }
            RESET => stage.reset(match r.other {
                0 => ResetReason::NewTrack,
                1 => ResetReason::Seek,
                2 => ResetReason::FormatChange,
                3 => ResetReason::Stop,
                _ => return Err(Error::InvalidSettings),
            }),
            UPDATE => {
                let settings = serde_json::from_slice(unsafe { r.data.bytes()? })
                    .map_err(|_| Error::InvalidSettings)?;
                stage.update(&settings)?;
            }
            DIAGNOSTICS => {
                r.reply = Buffer::owned(
                    serde_json::to_vec(&stage.diagnostics()).map_err(|_| Error::HostFailure)?,
                );
            }
            _ => return Err(Error::CapabilityMissing),
        }
        Ok(())
    })
}

#[macro_export]
macro_rules! export_rate {
    ($factory:expr, $manifest:expr $(, finalizer = $finalizer:expr)?) => {
        mod native_export_rate {
            unsafe extern "C" fn manifest() -> $crate::Buffer {
                $crate::Buffer::owned($manifest.as_bytes().to_vec())
            }
            unsafe extern "C" fn call(r: *mut $crate::Request) -> i32 {
                $(
                    if !r.is_null() && unsafe {r.cast::<u32>().read()} == size_of::<$crate::Request>() as u32 &&
                        matches!(unsafe {(*r).operation},$crate::finish::OPTIONS|$crate::finish::CREATE_FINALIZER|$crate::finish::FINISH_PCM|$crate::finish::DROP_FINALIZER){
                        return unsafe {$crate::finish::call(&$finalizer,r)};
                    }
                )?
                unsafe { $crate::rate::call(&$factory, r) }
            }
            static API: $crate::Api = $crate::Api {
                size: size_of::<$crate::Api>() as u32,
                version: $crate::ABI_VERSION,
                kind: $crate::rate::KIND,
                manifest,
                call,
                free: $crate::free,
            };
            #[unsafe(no_mangle)]
            pub extern "C" fn tune_audio_plugin_v1() -> *const $crate::Api {
                &API
            }
        }
    };
}
