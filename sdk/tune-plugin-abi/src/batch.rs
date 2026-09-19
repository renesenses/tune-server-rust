//! Batch ABI: JSON for control messages, borrowed i32 PCM for reads/writes.
//! HostApi lives for RUN_BATCH only; callbacks must be serialized by the host.
use crate::{Buffer, Format, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::ffi::c_void;
use tune_plugin_sdk::{Error, Settings, audio::AudioFormat, batch::*};
#[repr(C)]
pub struct HostApi {
    pub size: u32,
    pub version: u32,
    pub context: *mut c_void,
    pub control: unsafe extern "C" fn(*mut c_void, Buffer, *mut Buffer) -> i32,
    pub free_reply: unsafe extern "C" fn(Buffer),
    pub read: unsafe extern "C" fn(*mut c_void, u64, *mut i32, u64, *mut u64) -> i32,
    pub seek: unsafe extern "C" fn(*mut c_void, u64, u64) -> i32,
    pub close: unsafe extern "C" fn(*mut c_void, u64),
    pub write: unsafe extern "C" fn(*mut c_void, Buffer, *const i32, u64) -> i32,
}
#[derive(Serialize, Deserialize)]
pub struct Run {
    pub sources: Vec<SourceSelection>,
    pub settings: Settings,
}
struct RemoteHost {
    api: *mut HostApi,
}
// SAFETY: ABI callers promise synchronized callbacks; the host API remains
// alive until this synchronous batch invocation (and all its workers) finishes.
unsafe impl Send for RemoteHost {}
impl RemoteHost {
    fn call<T: DeserializeOwned>(&self, operation: &str, args: Value) -> Result<T, Error> {
        let mut request =
            serde_json::to_vec(&json!([operation, args])).map_err(|_| Error::InvalidSettings)?;
        let mut reply = Buffer::default();
        let api = unsafe { &*self.api };
        let status = unsafe {
            (api.control)(
                api.context,
                Buffer {
                    data: request.as_mut_ptr(),
                    len: request.len() as u64,
                },
                &mut reply,
            )
        };
        if status != 0 {
            return Err(crate::from_code(status));
        }
        let parsed = unsafe { reply.bytes() }
            .and_then(|bytes| serde_json::from_slice(bytes).map_err(|_| Error::HostFailure));
        unsafe { (api.free_reply)(reply) };
        parsed
    }
}
struct Reader {
    api: *mut HostApi,
    id: u64,
    format: AudioFormat,
}
// SAFETY: same synchronous job and synchronized host callback contract.
unsafe impl Send for Reader {}
impl PcmReader for Reader {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn read_frames(&mut self, samples: &mut [i32]) -> Result<usize, Error> {
        let api = unsafe { &*self.api };
        let mut frames = 0;
        let status = unsafe {
            (api.read)(
                api.context,
                self.id,
                samples.as_mut_ptr(),
                samples.len() as u64,
                &mut frames,
            )
        };
        if status != 0 {
            return Err(crate::from_code(status));
        }
        usize::try_from(frames).map_err(|_| Error::HostFailure)
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), Error> {
        let api = unsafe { &*self.api };
        let status = unsafe { (api.seek)(api.context, self.id, frame) };
        if status == 0 {
            Ok(())
        } else {
            Err(crate::from_code(status))
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        let api = unsafe { &*self.api };
        unsafe { (api.close)(api.context, self.id) }
    }
}
impl BatchHost for RemoteHost {
    fn resolve(&mut self, selections: &[SourceSelection]) -> Result<Vec<SourceHandle>, Error> {
        self.call("resolve", json!(selections))
    }
    fn codecs(&self) -> Vec<CodecCapability> {
        self.call("codecs", Value::Null).unwrap_or_default()
    }
    fn cancelled(&self) -> bool {
        self.call("cancelled", Value::Null).unwrap_or(true)
    }
    fn source_format(&mut self, source: &SourceHandle) -> Result<AudioFormat, Error> {
        self.call::<Format>("format", json!(source))?.audio()
    }
    fn open(&mut self, source: &SourceHandle) -> Result<Box<dyn PcmReader>, Error> {
        let (id, format): (u64, Format) = self.call("open", json!(source))?;
        Ok(Box::new(Reader {
            api: self.api,
            id,
            format: format.audio()?,
        }))
    }
    fn create_output(
        &mut self,
        source: &SourceHandle,
        options: &EncodeOptions,
        destination: &Destination,
    ) -> Result<WriterHandle, Error> {
        self.call("create", json!([source, options, destination]))
    }
    fn render_source(
        &mut self,
        source: &SourceHandle,
        output: &WriterHandle,
        options: &EncodeOptions,
    ) -> Result<(), Error> {
        self.call("render", json!([source, output, options]))
    }
    fn write_frames(&mut self, output: &WriterHandle, samples: &[i32]) -> Result<(), Error> {
        let api = unsafe { &*self.api };
        let mut handle = serde_json::to_vec(output).map_err(|_| Error::InvalidSettings)?;
        let status = unsafe {
            (api.write)(
                api.context,
                Buffer {
                    data: handle.as_mut_ptr(),
                    len: handle.len() as u64,
                },
                samples.as_ptr(),
                samples.len() as u64,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(crate::from_code(status))
        }
    }
    fn copy_metadata(&mut self, source: &SourceHandle, output: &WriterHandle) -> Result<(), Error> {
        self.call("metadata", json!([source, output]))
    }
    fn finish(&mut self, output: &WriterHandle) -> Result<ArtifactHandle, Error> {
        self.call("finish", json!(output))
    }
    fn abort(&mut self, output: &WriterHandle) {
        let _: Result<(), _> = self.call("abort", json!(output));
    }
    fn progress(&mut self, completed: usize, total: usize, current: &SourceHandle) {
        let _: Result<(), _> = self.call("progress", json!([completed, total, current]));
    }
}
/// # Safety
/// Valid Request and HostApi, buffers and callbacks must remain alive until
/// this returns. No callback may retain a borrowed pointer after returning.
pub unsafe fn plugin_call(tool: &dyn BatchTool, raw: *mut Request) -> i32 {
    crate::boundary(|| {
        if raw.is_null() {
            return Err(Error::InvalidState);
        }
        if unsafe { raw.cast::<u32>().read() } != std::mem::size_of::<Request>() as u32 {
            return Err(Error::InvalidState);
        }
        let r = unsafe { &mut *raw };
        if r.size != std::mem::size_of::<Request>() as u32
            || r.operation != crate::RUN_BATCH
            || r.host.is_null()
        {
            return Err(Error::InvalidState);
        }
        if unsafe { r.host.cast::<u32>().read() } != std::mem::size_of::<HostApi>() as u32 {
            return Err(Error::CapabilityMissing);
        }
        let api = unsafe { &*r.host };
        if api.size != std::mem::size_of::<HostApi>() as u32 || api.version != crate::ABI_VERSION {
            return Err(Error::CapabilityMissing);
        }
        let input: Run = serde_json::from_slice(unsafe { r.data.bytes()? })
            .map_err(|_| Error::InvalidSettings)?;
        let result = tool.run(
            &mut RemoteHost { api: r.host },
            &input.sources,
            &input.settings,
        )?;
        r.reply = Buffer::owned(serde_json::to_vec(&result).map_err(|_| Error::HostFailure)?);
        Ok(())
    })
}
