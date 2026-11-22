//! Optional final PCM quantization. The device callback owns an independent
//! handle; the filter and the finalizer never share mutable stream state.
use super::*;
pub const OPTIONS: u32 = 14;
pub const CREATE_FINALIZER: u32 = 11;
pub const FINISH_PCM: u32 = 12;
pub const DROP_FINALIZER: u32 = 13;
pub trait FinalPcm: Send {
    fn process(&mut self, words: &mut [f64]);
}
pub trait FinalPcmFactory {
    fn options(&self) -> Result<Settings, Error> {
        Ok(Settings::Null)
    }
    fn prepare_final(
        &self,
        input: AudioFormat,
        rate: u32,
        transport_bits: u16,
        settings: &Settings,
    ) -> Result<Box<dyn FinalPcm>, Error>;
}
struct Owned(Box<dyn FinalPcm>);

/// # Safety
/// CREATE lends JSON settings. FINISH lends a single mutable aligned buffer
/// of initialized f64 words. Handles are live, exclusive, and created here.
pub unsafe fn call(factory: &dyn FinalPcmFactory, raw: *mut Request) -> i32 {
    boundary(|| {
        if raw.is_null() || unsafe { raw.cast::<u32>().read() } != size_of::<Request>() as u32 {
            return Err(Error::InvalidState);
        }
        let r = unsafe { &mut *raw };
        match r.operation {
            OPTIONS => {
                r.reply = Buffer::owned(
                    serde_json::to_vec(&factory.options()?).map_err(|_| Error::HostFailure)?,
                );
            }
            CREATE_FINALIZER => {
                let settings = serde_json::from_slice(unsafe { r.data.bytes()? })
                    .map_err(|_| Error::InvalidSettings)?;
                let instance = factory.prepare_final(
                    r.format.audio()?,
                    u32::try_from(r.other).map_err(|_| Error::InvalidFormat)?,
                    u16::try_from(r.flags).map_err(|_| Error::InvalidFormat)?,
                    &settings,
                )?;
                r.handle = Box::into_raw(Box::new(Owned(instance))) as usize as u64;
            }
            _ if r.handle == 0 => return Err(Error::InvalidState),
            FINISH_PCM => {
                if r.data.len == 0 {
                    return Ok(());
                }
                if r.data.len > isize::MAX as u64
                    || !r.data.len.is_multiple_of(8)
                    || r.data.data.is_null()
                    || !(r.data.data as usize).is_multiple_of(align_of::<f64>())
                {
                    return Err(Error::InvalidFormat);
                }
                let words = unsafe {
                    std::slice::from_raw_parts_mut(
                        r.data.data.cast::<f64>(),
                        r.data.len as usize / 8,
                    )
                };
                unsafe { &mut *(r.handle as usize as *mut Owned) }
                    .0
                    .process(words);
            }
            DROP_FINALIZER => {
                drop(unsafe { Box::from_raw(r.handle as usize as *mut Owned) });
                r.handle = 0;
            }
            _ => return Err(Error::CapabilityMissing),
        }
        Ok(())
    })
}
