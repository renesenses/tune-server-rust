//! Native ABI v1. Only repr(C) fixed-width scalars, opaque handles and borrowed
//! buffers cross this boundary. All functions are synchronous. The caller owns
//! request buffers; replies are freed by the originating library's `free`.
//! Loaded libraries are trusted code, NOT a sandbox. See SAFETY.md.
#![deny(unsafe_op_in_unsafe_fn)]
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
};
use tune_plugin_sdk::{Error, Settings, audio::*};
pub mod batch;
pub const ABI_VERSION: u32 = 1;
pub const DSP: u32 = 1;
pub const BATCH: u32 = 2;
pub const CREATE: u32 = 1;
pub const PROCESS: u32 = 2;
pub const UPDATE: u32 = 3;
pub const RESET: u32 = 4;
pub const INHERIT: u32 = 5;
pub const DIAGNOSTICS: u32 = 6;
pub const DESTROY: u32 = 7;
pub const RUN_BATCH: u32 = 8;
pub const DRAIN: u32 = 9;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub data: *mut u8,
    pub len: u64,
}
impl Default for Buffer {
    fn default() -> Self {
        Self {
            data: std::ptr::null_mut(),
            len: 0,
        }
    }
}
impl Buffer {
    pub fn owned(bytes: Vec<u8>) -> Self {
        let bytes = bytes.into_boxed_slice();
        let len = bytes.len() as u64;
        Self {
            data: Box::into_raw(bytes) as *mut u8,
            len,
        }
    }
    /// # Safety
    /// `data` must point to `len` readable bytes for the returned borrow.
    pub unsafe fn bytes<'a>(&self) -> Result<&'a [u8], Error> {
        let len = usize::try_from(self.len).map_err(|_| Error::InvalidSettings)?;
        if len > 64 * 1024 * 1024 || (len > 0 && self.data.is_null()) {
            return Err(Error::InvalidSettings);
        }
        if len == 0 {
            return Ok(&[]);
        }
        Ok(unsafe { std::slice::from_raw_parts(self.data, len) })
    }
}
/// # Safety
/// Buffer must be a still-owned reply from this library, freed exactly once.
pub unsafe extern "C" fn free(buffer: Buffer) {
    if !buffer.data.is_null() {
        let pointer = std::ptr::slice_from_raw_parts_mut(buffer.data, buffer.len as usize);
        drop(unsafe { Box::from_raw(pointer) });
    }
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: u16,
}
impl Format {
    pub fn from_audio(f: AudioFormat) -> Self {
        Self {
            sample_rate: f.sample_rate(),
            channels: f.channels(),
            encoding: match f.encoding() {
                SampleEncoding::S16 => 1,
                SampleEncoding::S24Le => 2,
                SampleEncoding::S32 => 3,
                SampleEncoding::F32 => 4,
                SampleEncoding::F64 => 5,
            },
        }
    }
    pub fn audio(self) -> Result<AudioFormat, Error> {
        AudioFormat::new(
            self.sample_rate,
            match self.channels {
                1 => ChannelLayout::Mono,
                2 => ChannelLayout::Stereo,
                n => ChannelLayout::Discrete(n),
            },
            match self.encoding {
                1 => SampleEncoding::S16,
                2 => SampleEncoding::S24Le,
                3 => SampleEncoding::S32,
                4 => SampleEncoding::F32,
                5 => SampleEncoding::F64,
                _ => return Err(Error::InvalidFormat),
            },
        )
    }
    pub fn sample_bytes(self) -> Result<usize, Error> {
        match self.encoding {
            1 => Ok(2),
            2 => Ok(3),
            3 | 4 => Ok(4),
            5 => Ok(8),
            _ => Err(Error::InvalidFormat),
        }
    }
}
#[repr(C)]
pub struct Request {
    pub size: u32,
    pub operation: u32,
    pub handle: u64,
    pub other: u64,
    pub format: Format,
    pub frames: u32,
    pub flags: u32,
    pub data: Buffer,
    pub reply: Buffer,
    pub zone_id: i64,
    pub generation: u64,
    pub position_frames: u64,
    pub clipping_seen: u64,
    pub clipping_total: u64,
    pub clipping_excess: u64,
    pub clipping_peak: u64,
    pub clipping_first: u64,
    pub clipped: u64,
    pub non_finite: u64,
    pub host: *mut batch::HostApi,
}
impl Request {
    pub fn new(operation: u32) -> Self {
        Self {
            size: std::mem::size_of::<Self>() as u32,
            operation,
            handle: 0,
            other: 0,
            format: Format::default(),
            frames: 0,
            flags: 0,
            data: Buffer::default(),
            reply: Buffer::default(),
            zone_id: 0,
            generation: 0,
            position_frames: 0,
            clipping_seen: 0,
            clipping_total: 0,
            clipping_excess: 0,
            clipping_peak: 0,
            clipping_first: u64::MAX,
            clipped: 0,
            non_finite: 0,
            host: std::ptr::null_mut(),
        }
    }
}
#[repr(C)]
pub struct Api {
    pub size: u32,
    pub version: u32,
    pub kind: u32,
    pub manifest: unsafe extern "C" fn() -> Buffer,
    pub call: unsafe extern "C" fn(*mut Request) -> i32,
    pub free: unsafe extern "C" fn(Buffer),
}
pub fn boundary(f: impl FnOnce() -> Result<(), Error>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => error_code(e),
        Err(_) => 255,
    }
}
pub fn error_code(e: Error) -> i32 {
    match e {
        Error::InvalidFormat => 1,
        Error::IncompleteFrame => 2,
        Error::BlockTooLarge => 3,
        Error::UnsupportedFormat => 4,
        Error::InvalidSettings => 5,
        Error::NonFinite => 6,
        Error::Cancelled => 7,
        Error::CapabilityMissing => 8,
        Error::InvalidState => 9,
        Error::InvalidObservation => 10,
        Error::HostFailure => 11,
        Error::SilenceOnly => 12,
    }
}
pub fn from_code(code: i32) -> Error {
    match code {
        1 => Error::InvalidFormat,
        2 => Error::IncompleteFrame,
        3 => Error::BlockTooLarge,
        4 => Error::UnsupportedFormat,
        5 => Error::InvalidSettings,
        6 => Error::NonFinite,
        7 => Error::Cancelled,
        8 => Error::CapabilityMissing,
        9 => Error::InvalidState,
        10 => Error::InvalidObservation,
        12 => Error::SilenceOnly,
        _ => Error::HostFailure,
    }
}
struct Instance {
    processor: Box<dyn Processor>,
    format: AudioFormat,
    max_frames: usize,
}
/// # Safety
/// Request must be uniquely borrowed, correctly sized and alive for the call.
/// All nonzero handles must come from CREATE in this same library and remain
/// alive; PROCESS/UPDATE/RESET/DESTROY require exclusive access to the handle.
pub unsafe fn dsp_call(factory: &dyn DspFactory, raw: *mut Request) -> i32 {
    boundary(|| {
        if raw.is_null() {
            return Err(Error::InvalidState);
        }
        if unsafe { raw.cast::<u32>().read() } != std::mem::size_of::<Request>() as u32 {
            return Err(Error::InvalidState);
        }
        let r = unsafe { &mut *raw };
        if r.size != std::mem::size_of::<Request>() as u32 {
            return Err(Error::InvalidState);
        }
        if r.operation == CREATE {
            let settings: Settings = serde_json::from_slice(unsafe { r.data.bytes()? })
                .map_err(|_| Error::InvalidSettings)?;
            let format = r.format.audio()?;
            let processor = factory.prepare(format, r.frames as usize, &settings)?;
            r.handle = Box::into_raw(Box::new(Instance {
                processor,
                format,
                max_frames: r.frames as usize,
            })) as usize as u64;
            return Ok(());
        }
        if r.handle == 0 {
            return Err(Error::InvalidState);
        }
        if r.operation == DESTROY {
            drop(unsafe { Box::from_raw(r.handle as usize as *mut Instance) });
            r.handle = 0;
            return Ok(());
        }
        let instance = unsafe { &mut *(r.handle as usize as *mut Instance) };
        match r.operation {
            PROCESS | DRAIN => {
                if r.format.audio()? != instance.format {
                    return Err(Error::InvalidFormat);
                }
                let expected = (r.frames as usize)
                    .checked_mul(usize::from(r.format.channels))
                    .and_then(|n| n.checked_mul(r.format.sample_bytes().ok()?))
                    .ok_or(Error::BlockTooLarge)?;
                if r.frames as usize > instance.max_frames
                    || r.data.len != expected as u64
                    || (expected > 0 && r.data.data.is_null())
                {
                    return Err(Error::IncompleteFrame);
                }
                let ptr = if expected == 0 {
                    std::ptr::NonNull::<u64>::dangling().as_ptr().cast::<u8>()
                } else {
                    r.data.data
                };
                let samples = unsafe { borrow_samples(ptr, expected, r.format.encoding)? };
                let mut block = AudioBlock::new(instance.format, samples, instance.max_frames)?;
                if r.operation == PROCESS {
                    let report = instance.processor.process(
                        &mut block,
                        BlockContext {
                            zone_id: r.zone_id,
                            generation: r.generation,
                            position_frames: r.position_frames,
                        },
                    )?;
                    r.flags = u32::from(report.changed);
                    r.clipped = report.clipped_samples;
                    r.non_finite = report.non_finite_samples;
                    if let Some(clipping) = report.clipping {
                        r.flags |= 2;
                        r.clipping_seen = clipping.samples_seen;
                        r.clipping_total = clipping.clipped_samples;
                        r.clipping_excess = clipping.max_excess_lsb;
                        r.clipping_peak = clipping.max_peak_bits;
                        r.clipping_first = clipping.first_clip.unwrap_or(u64::MAX);
                    }
                } else {
                    let report = instance.processor.drain(&mut block)?;
                    r.frames = report.frames_written as u32;
                    r.flags = u32::from(report.complete);
                }
            }
            UPDATE => {
                let settings = serde_json::from_slice(unsafe { r.data.bytes()? })
                    .map_err(|_| Error::InvalidSettings)?;
                instance.processor.update(&settings)?;
            }
            RESET => instance.processor.reset(match r.other {
                0 => ResetReason::NewTrack,
                1 => ResetReason::Seek,
                2 => ResetReason::FormatChange,
                3 => ResetReason::Stop,
                _ => return Err(Error::InvalidSettings),
            }),
            INHERIT => {
                if r.other == 0 || r.other == r.handle {
                    return Err(Error::InvalidState);
                }
                let previous = unsafe { &*(r.other as usize as *const Instance) };
                instance.processor.inherit_from(&*previous.processor)?;
            }
            DIAGNOSTICS => {
                r.other = u64::from(instance.processor.latency_frames());
                r.reply = Buffer::owned(
                    serde_json::to_vec(&instance.processor.diagnostics())
                        .map_err(|_| Error::HostFailure)?,
                );
            }
            _ => return Err(Error::InvalidSettings),
        }
        Ok(())
    })
}
unsafe fn borrow_samples<'a>(
    ptr: *mut u8,
    bytes: usize,
    encoding: u16,
) -> Result<SamplesMut<'a>, Error> {
    macro_rules! typed {
        ($t:ty,$v:ident) => {{
            if !(ptr as usize).is_multiple_of(std::mem::align_of::<$t>()) {
                return Err(Error::InvalidFormat);
            }
            SamplesMut::$v(unsafe {
                std::slice::from_raw_parts_mut(ptr as *mut $t, bytes / std::mem::size_of::<$t>())
            })
        }};
    }
    Ok(match encoding {
        1 => typed!(i16, S16),
        2 => SamplesMut::S24Le(unsafe { std::slice::from_raw_parts_mut(ptr, bytes) }),
        3 => typed!(i32, S32),
        4 => typed!(f32, F32),
        5 => typed!(f64, F64),
        _ => return Err(Error::InvalidFormat),
    })
}
#[macro_export]
macro_rules! export_dsp {
    ($factory:expr,$manifest:expr) => {
        mod native_export {
            unsafe extern "C" fn manifest() -> $crate::Buffer {
                $crate::Buffer::owned($manifest.as_bytes().to_vec())
            }
            unsafe extern "C" fn call(request: *mut $crate::Request) -> i32 {
                unsafe { $crate::dsp_call(&$factory, request) }
            }
            static API: $crate::Api = $crate::Api {
                size: std::mem::size_of::<$crate::Api>() as u32,
                version: $crate::ABI_VERSION,
                kind: $crate::DSP,
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
#[macro_export]
macro_rules! export_batch {
    ($tool:expr,$manifest:expr) => {
        mod native_export {
            unsafe extern "C" fn manifest() -> $crate::Buffer {
                $crate::Buffer::owned($manifest.as_bytes().to_vec())
            }
            unsafe extern "C" fn call(request: *mut $crate::Request) -> i32 {
                unsafe { $crate::batch::plugin_call(&$tool, request) }
            }
            static API: $crate::Api = $crate::Api {
                size: std::mem::size_of::<$crate::Api>() as u32,
                version: $crate::ABI_VERSION,
                kind: $crate::BATCH,
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
// Keep the opaque pointer type visible to downstream C header generators.
pub type Opaque = *mut c_void;
