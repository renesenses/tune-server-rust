//! Verified native audio packages and pinned library lifetimes. Native plugins
//! execute trusted code in process. Installation/loading is control-plane only.
#![deny(unsafe_op_in_unsafe_fn)]
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, OnceLock, RwLock},
};
use tune_plugin_abi as abi;
use tune_plugin_sdk::{Error, Settings, audio::*, batch::*};
mod batch_host;
pub mod package;

pub struct Library {
    library: libloading::Library,
    api: *const abi::Api,
    pub manifest: tune_plugin_sdk::manifest::Manifest,
}
// SAFETY: Api is immutable library-owned static data. It stays mapped for the
// Arc lifetime. Individual processor handles are uniquely owned and Send;
// every batch invocation gets an independent synchronized host bridge.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}
impl Library {
    /// # Safety
    /// Caller must verify code authenticity and integrity before calling. A
    /// native library can execute arbitrary code during load, before ABI checks.
    pub unsafe fn load_trusted(path: &Path) -> Result<Arc<Self>, String> {
        let library = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
        let entry: libloading::Symbol<unsafe extern "C" fn() -> *const abi::Api> =
            unsafe { library.get(b"tune_audio_plugin_v1\0") }.map_err(|e| e.to_string())?;
        let pointer = unsafe { entry() };
        if pointer.is_null() {
            return Err("null plugin entry".into());
        }
        if unsafe { pointer.cast::<u32>().read() } != std::mem::size_of::<abi::Api>() as u32 {
            return Err("incompatible audio ABI size".into());
        }
        let api = unsafe { &*pointer };
        if api.size != std::mem::size_of::<abi::Api>() as u32
            || api.version != abi::ABI_VERSION
            || ![abi::DSP, abi::BATCH].contains(&api.kind)
        {
            return Err("incompatible audio ABI".into());
        }
        let raw = unsafe { (api.manifest)() };
        let parsed = unsafe { raw.bytes() }
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<tune_plugin_sdk::manifest::Manifest>(bytes)
                    .map_err(|e| e.to_string())
            });
        unsafe { (api.free)(raw) };
        let manifest = parsed?;
        manifest
            .validate()
            .map_err(|e| format!("manifest: {e:?}"))?;
        if (api.kind == abi::DSP) != (manifest.kind == tune_plugin_sdk::manifest::PluginKind::Dsp) {
            return Err("manifest kind differs from ABI".into());
        }
        Ok(Arc::new(Self {
            library,
            api: pointer,
            manifest,
        }))
    }
    fn api(&self) -> &abi::Api {
        let _pin = &self.library;
        unsafe { &*self.api }
    }
    fn call(&self, request: &mut abi::Request) -> Result<(), Error> {
        let status = unsafe { (self.api().call)(request) };
        if status == 0 {
            Ok(())
        } else {
            Err(abi::from_code(status))
        }
    }
    fn reply<T: serde::de::DeserializeOwned>(&self, buffer: abi::Buffer) -> Result<T, Error> {
        let parsed = unsafe { buffer.bytes() }
            .and_then(|bytes| serde_json::from_slice(bytes).map_err(|_| Error::HostFailure));
        unsafe { (self.api().free)(buffer) };
        parsed
    }
    pub fn prepare(
        self: &Arc<Self>,
        format: AudioFormat,
        max_frames: usize,
        settings: &Settings,
    ) -> Result<NativeProcessor, Error> {
        if self.api().kind != abi::DSP {
            return Err(Error::CapabilityMissing);
        }
        let mut data = serde_json::to_vec(settings).map_err(|_| Error::InvalidSettings)?;
        let mut request = abi::Request::new(abi::CREATE);
        request.format = abi::Format::from_audio(format);
        request.frames = u32::try_from(max_frames).map_err(|_| Error::BlockTooLarge)?;
        request.data = abi::Buffer {
            data: data.as_mut_ptr(),
            len: data.len() as u64,
        };
        self.call(&mut request)?;
        if request.handle == 0 {
            return Err(Error::HostFailure);
        }
        Ok(NativeProcessor {
            library: self.clone(),
            handle: request.handle,
            format,
            max_frames,
        })
    }
}
pub struct NativeProcessor {
    library: Arc<Library>,
    handle: u64,
    format: AudioFormat,
    max_frames: usize,
}
impl Drop for NativeProcessor {
    fn drop(&mut self) {
        let mut request = abi::Request::new(abi::DESTROY);
        request.handle = self.handle;
        let _ = self.library.call(&mut request);
    }
}
impl NativeProcessor {
    fn request(&self, operation: u32) -> abi::Request {
        let mut r = abi::Request::new(operation);
        r.handle = self.handle;
        r.format = abi::Format::from_audio(self.format);
        r
    }
    fn block_request(
        &self,
        block: &mut AudioBlock<'_>,
        operation: u32,
    ) -> Result<abi::Request, Error> {
        if block.format() != self.format || block.frames() > self.max_frames {
            return Err(Error::InvalidFormat);
        }
        let mut r = self.request(operation);
        r.frames = block.frames() as u32;
        r.data = match block.samples_mut() {
            SamplesMut::S16(s) => abi::Buffer {
                data: s.as_mut_ptr().cast(),
                len: std::mem::size_of_val(s) as u64,
            },
            SamplesMut::S24Le(s) => abi::Buffer {
                data: s.as_mut_ptr(),
                len: s.len() as u64,
            },
            SamplesMut::S32(s) => abi::Buffer {
                data: s.as_mut_ptr().cast(),
                len: std::mem::size_of_val(s) as u64,
            },
            SamplesMut::F32(s) => abi::Buffer {
                data: s.as_mut_ptr().cast(),
                len: std::mem::size_of_val(s) as u64,
            },
            SamplesMut::F64(s) => abi::Buffer {
                data: s.as_mut_ptr().cast(),
                len: std::mem::size_of_val(s) as u64,
            },
        };
        Ok(r)
    }
}
impl Processor for NativeProcessor {
    fn process(
        &mut self,
        block: &mut AudioBlock<'_>,
        context: BlockContext,
    ) -> Result<ProcessReport, Error> {
        let mut r = self.block_request(block, abi::PROCESS)?;
        r.zone_id = context.zone_id;
        r.generation = context.generation;
        r.position_frames = context.position_frames;
        self.library.call(&mut r)?;
        Ok(ProcessReport {
            changed: r.flags & 1 != 0,
            clipping: (r.flags & 2 != 0).then_some(ClippingStats {
                samples_seen: r.clipping_seen,
                clipped_samples: r.clipping_total,
                max_excess_lsb: r.clipping_excess,
                max_peak_bits: r.clipping_peak,
                first_clip: (r.clipping_first != u64::MAX).then_some(r.clipping_first),
            }),
            clipped_samples: r.clipped,
            non_finite_samples: r.non_finite,
        })
    }
    fn update(&mut self, settings: &Settings) -> Result<(), Error> {
        let mut data = serde_json::to_vec(settings).map_err(|_| Error::InvalidSettings)?;
        let mut r = self.request(abi::UPDATE);
        r.data = abi::Buffer {
            data: data.as_mut_ptr(),
            len: data.len() as u64,
        };
        self.library.call(&mut r)
    }
    fn inherit_from(&mut self, previous: &dyn Processor) -> Result<(), Error> {
        let previous = (previous as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .ok_or(Error::InvalidState)?;
        if !Arc::ptr_eq(&self.library, &previous.library) {
            return Err(Error::InvalidState);
        }
        let mut r = self.request(abi::INHERIT);
        r.other = previous.handle;
        self.library.call(&mut r)
    }
    fn diagnostics(&self) -> Settings {
        let mut r = self.request(abi::DIAGNOSTICS);
        if self.library.call(&mut r).is_err() {
            return Settings::Null;
        }
        self.library.reply(r.reply).unwrap_or(Settings::Null)
    }
    fn reset(&mut self, reason: ResetReason) {
        let mut r = self.request(abi::RESET);
        r.other = match reason {
            ResetReason::NewTrack => 0,
            ResetReason::Seek => 1,
            ResetReason::FormatChange => 2,
            ResetReason::Stop => 3,
        };
        let _ = self.library.call(&mut r);
    }
    fn latency_frames(&self) -> u32 {
        let mut r = self.request(abi::DIAGNOSTICS);
        if self.library.call(&mut r).is_err() {
            return 0;
        }
        let _: Result<Settings, _> = self.library.reply(r.reply);
        r.other as u32
    }
    fn drain(&mut self, block: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        let capacity = block.frames();
        let mut r = self.block_request(block, abi::DRAIN)?;
        self.library.call(&mut r)?;
        if r.frames as usize > capacity {
            return Err(Error::HostFailure);
        }
        Ok(DrainReport {
            frames_written: r.frames as usize,
            complete: r.flags != 0,
        })
    }
}
pub struct NativeBatch(pub Arc<Library>);
impl BatchTool for NativeBatch {
    fn run(
        &self,
        host: &mut dyn BatchHost,
        sources: &[SourceSelection],
        settings: &Settings,
    ) -> Result<JobResult, Error> {
        batch_host::run(&self.0, host, sources, settings)
    }
}
static REGISTRY: OnceLock<RwLock<BTreeMap<String, Arc<Library>>>> = OnceLock::new();
pub fn provider(id: &str) -> Option<Arc<Library>> {
    REGISTRY.get()?.read().ok()?.get(id).cloned()
}
/// Identifiers of every registered provider, sorted. Hosts use it to find
/// installed packages that are not one of their compiled-in slots.
pub fn provider_ids() -> Vec<String> {
    REGISTRY
        .get()
        .and_then(|registry| registry.read().ok().map(|r| r.keys().cloned().collect()))
        .unwrap_or_default()
}
/// Register before playback/jobs start. Replacing the registry entry does not
/// unload the previous library while existing processors still own it.
pub fn register(library: Arc<Library>) -> Result<(), Error> {
    REGISTRY
        .get_or_init(Default::default)
        .write()
        .map_err(|_| Error::InvalidState)?
        .insert(library.manifest.id.clone(), library);
    Ok(())
}
pub mod stage;
static FAILURES: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
pub fn record_failure(id: &str, error: String) {
    if let Ok(mut failures) = FAILURES.get_or_init(Default::default).write() {
        failures.insert(id.into(), error);
    }
}
pub fn failure(id: &str) -> Option<String> {
    FAILURES.get()?.read().ok()?.get(id).cloned()
}
