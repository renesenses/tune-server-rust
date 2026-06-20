//! WASAPI Exclusive mode output for Windows bit-perfect audio.
//!
//! Uses the Windows Audio Session API (WASAPI) in exclusive mode
//! (`AUDCLNT_SHAREMODE_EXCLUSIVE`) to bypass the Windows audio mixer
//! and send PCM directly to the DAC at the source sample rate.
//!
//! This avoids the system resampler (typically 48kHz) and provides
//! bit-perfect output similar to ASIO but without requiring a
//! third-party ASIO driver.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tracing::{debug, info};

use super::local::RingBuf;

// ---------------------------------------------------------------------------
// Windows COM/WASAPI FFI declarations
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod ffi {
    #![allow(non_snake_case, non_camel_case_types, dead_code)]

    use std::ffi::c_void;

    pub type HRESULT = i32;
    pub type HANDLE = *mut c_void;
    pub type DWORD = u32;
    pub type UINT32 = u32;
    pub type WORD = u16;
    pub type REFERENCE_TIME = i64;
    pub type LPUNKNOWN = *mut c_void;

    pub const S_OK: HRESULT = 0;
    pub const S_FALSE: HRESULT = 1;
    pub const COINIT_MULTITHREADED: u32 = 0x0;
    pub const CLSCTX_ALL: u32 = 0x17;
    pub const STGM_READ: u32 = 0;
    pub const AUDCLNT_SHAREMODE_EXCLUSIVE: u32 = 1;
    pub const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x00040000;
    pub const WAVE_FORMAT_PCM: u16 = 1;
    pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    pub const INFINITE: u32 = 0xFFFFFFFF;
    pub const WAIT_OBJECT_0: u32 = 0;

    // Render endpoint
    pub const E_DATA_FLOW_RENDER: u32 = 0;
    pub const E_ROLE_MULTIMEDIA: u32 = 1;

    // KSDATAFORMAT_SUBTYPE_PCM {00000001-0000-0010-8000-00AA00389B71}
    pub const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID {
        data1: 0x00000001,
        data2: 0x0000,
        data3: 0x0010,
        data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    };

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct GUID {
        pub data1: u32,
        pub data2: u16,
        pub data3: u16,
        pub data4: [u8; 8],
    }

    // CLSID_MMDeviceEnumerator {BCDE0395-E52F-467C-8E3D-C4579291692E}
    pub const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID {
        data1: 0xBCDE0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };

    // IID_IMMDeviceEnumerator {A95664D2-9614-4F35-A746-DE8DB63617E6}
    pub const IID_IMMDEVICE_ENUMERATOR: GUID = GUID {
        data1: 0xA95664D2,
        data2: 0x9614,
        data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };

    // IID_IAudioClient {1CB9AD4C-DBFA-4c32-B178-C2F568A703B2}
    pub const IID_IAUDIOCLIENT: GUID = GUID {
        data1: 0x1CB9AD4C,
        data2: 0xDBFA,
        data3: 0x4C32,
        data4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
    };

    // IID_IAudioRenderClient {F294ACFC-3146-4483-A7BF-ADDCA7C260E2}
    pub const IID_IAUDIO_RENDER_CLIENT: GUID = GUID {
        data1: 0xF294ACFC,
        data2: 0x3146,
        data3: 0x4483,
        data4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
    };

    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    pub struct WAVEFORMATEX {
        pub wFormatTag: WORD,
        pub nChannels: WORD,
        pub nSamplesPerSec: DWORD,
        pub nAvgBytesPerSec: DWORD,
        pub nBlockAlign: WORD,
        pub wBitsPerSample: WORD,
        pub cbSize: WORD,
    }

    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    pub struct WAVEFORMATEXTENSIBLE {
        pub Format: WAVEFORMATEX,
        pub Samples: WORD, // wValidBitsPerSample or wSamplesPerBlock
        pub dwChannelMask: DWORD,
        pub SubFormat: GUID,
    }

    // COM vtable offsets (IUnknown: 0=QueryInterface, 1=AddRef, 2=Release)
    // IMMDeviceEnumerator: 3=EnumAudioEndpoints, 4=GetDefaultAudioEndpoint, 5=GetDevice
    // IMMDevice: 3=Activate, 4=OpenPropertyStore, 5=GetId, 6=GetState
    // IAudioClient: 3=Initialize, 4=GetBufferSize, 5=GetStreamLatency, 6=GetCurrentPadding,
    //               7=IsFormatSupported, 8=GetMixFormat, 9=GetDevicePeriod, 10=Start,
    //               11=Stop, 12=Reset, 13=SetEventHandle, 14=GetService
    // IAudioRenderClient: 3=GetBuffer, 4=ReleaseBuffer
    // IPropertyStore: 3=GetCount, 4=GetAt, 5=GetValue

    unsafe extern "system" {
        pub fn CoInitializeEx(pvReserved: *const c_void, dwCoInit: u32) -> HRESULT;
        pub fn CoCreateInstance(
            rclsid: *const GUID,
            pUnkOuter: LPUNKNOWN,
            dwClsContext: u32,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> HRESULT;
        pub fn CoTaskMemFree(pv: *mut c_void);
        pub fn CreateEventW(
            lpEventAttributes: *const c_void,
            bManualReset: i32,
            bInitialState: i32,
            lpName: *const u16,
        ) -> HANDLE;
        pub fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: DWORD) -> DWORD;
        pub fn CloseHandle(hObject: HANDLE) -> i32;
    }

    /// Helper to call a COM vtable method by index.
    /// # Safety
    /// Caller must ensure `obj` is a valid COM interface pointer and the
    /// vtable index/signature match.
    pub unsafe fn vtable_call<T>(obj: *mut c_void, index: usize) -> *const T {
        let vtable = *(obj as *const *const *const c_void);
        *vtable.add(index) as *const T
    }

    /// Call IUnknown::Release on a COM object.
    #[allow(unused_unsafe)]
    pub unsafe fn release(obj: *mut c_void) {
        if !obj.is_null() {
            type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
            unsafe {
                let vtable = *(obj as *const *const *const c_void);
                let release: ReleaseFn = std::mem::transmute(*vtable.add(2));
                release(obj);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct WasapiExclusiveOutput {
    device_name: String,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    ring: Arc<RingBuf>,
    volume: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    audio_client: *mut std::ffi::c_void,
    #[cfg(target_os = "windows")]
    render_client: *mut std::ffi::c_void,
    #[cfg(target_os = "windows")]
    event_handle: ffi::HANDLE,
    #[cfg(target_os = "windows")]
    buffer_frame_count: u32,
    running: Arc<AtomicBool>,
    render_thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiExclusiveOutput {
    /// Try to open the default audio device in WASAPI Exclusive mode
    /// at the given sample rate and bit depth.
    #[cfg(target_os = "windows")]
    pub fn new(
        device_name: &str,
        sample_rate: u32,
        bit_depth: u32,
        channels: u32,
        ring: Arc<RingBuf>,
        volume: Arc<AtomicU32>,
        paused: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        use ffi::*;
        use std::ffi::c_void;
        use std::ptr;

        unsafe {
            CoInitializeEx(ptr::null(), COINIT_MULTITHREADED);

            // 1. Create MMDeviceEnumerator
            let mut enumerator: *mut c_void = ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_MMDEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMMDEVICE_ENUMERATOR,
                &mut enumerator,
            );
            if hr != S_OK {
                return Err(format!(
                    "CoCreateInstance(MMDeviceEnumerator) failed: 0x{hr:08X}"
                ));
            }

            // 2. GetDefaultAudioEndpoint(eRender, eMultimedia)
            let mut device: *mut c_void = ptr::null_mut();
            {
                type GetDefaultFn =
                    unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT;
                let vtable = *(enumerator as *const *const *const c_void);
                let get_default: GetDefaultFn = std::mem::transmute(*vtable.add(4));
                let hr = get_default(
                    enumerator,
                    E_DATA_FLOW_RENDER,
                    E_ROLE_MULTIMEDIA,
                    &mut device,
                );
                if hr != S_OK {
                    release(enumerator);
                    return Err(format!("GetDefaultAudioEndpoint failed: 0x{hr:08X}"));
                }
            }

            // 3. Activate IAudioClient
            let mut audio_client: *mut c_void = ptr::null_mut();
            {
                type ActivateFn = unsafe extern "system" fn(
                    *mut c_void,
                    *const GUID,
                    u32,
                    *mut c_void,
                    *mut *mut c_void,
                ) -> HRESULT;
                let vtable = *(device as *const *const *const c_void);
                let activate: ActivateFn = std::mem::transmute(*vtable.add(3));
                let hr = activate(
                    device,
                    &IID_IAUDIOCLIENT,
                    CLSCTX_ALL,
                    ptr::null_mut(),
                    &mut audio_client,
                );
                release(device);
                release(enumerator);
                if hr != S_OK {
                    return Err(format!(
                        "IMMDevice::Activate(IAudioClient) failed: 0x{hr:08X}"
                    ));
                }
            }

            // 4. Build WAVEFORMATEXTENSIBLE for our desired format
            let block_align = (channels as u16) * (bit_depth as u16 / 8);
            let avg_bytes = sample_rate * block_align as u32;
            let wfx = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                    nChannels: channels as u16,
                    nSamplesPerSec: sample_rate,
                    nAvgBytesPerSec: avg_bytes,
                    nBlockAlign: block_align,
                    wBitsPerSample: bit_depth as u16,
                    cbSize: 22,
                },
                Samples: bit_depth as u16,
                dwChannelMask: if channels == 2 {
                    0x3
                } else {
                    (1u32 << channels) - 1
                },
                SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
            };

            // 5. Check if format is supported in exclusive mode
            {
                type IsFormatFn = unsafe extern "system" fn(
                    *mut c_void,
                    u32,
                    *const WAVEFORMATEX,
                    *mut *mut WAVEFORMATEX,
                ) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let is_format: IsFormatFn = std::mem::transmute(*vtable.add(7));
                let hr = is_format(
                    audio_client,
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    &wfx.Format as *const WAVEFORMATEX,
                    ptr::null_mut(),
                );
                if hr != S_OK && hr != S_FALSE {
                    info!(
                        sample_rate,
                        bit_depth,
                        channels,
                        hr = format!("0x{hr:08X}"),
                        "wasapi_exclusive_format_not_supported"
                    );
                    release(audio_client);
                    return Err(format!(
                        "WASAPI Exclusive: format {channels}ch {bit_depth}bit {sample_rate}Hz not supported (0x{hr:08X})"
                    ));
                }
            }

            // 6. Get device period for exclusive mode
            let mut default_period: REFERENCE_TIME = 0;
            let mut min_period: REFERENCE_TIME = 0;
            {
                type GetPeriodFn = unsafe extern "system" fn(
                    *mut c_void,
                    *mut REFERENCE_TIME,
                    *mut REFERENCE_TIME,
                ) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let get_period: GetPeriodFn = std::mem::transmute(*vtable.add(9));
                get_period(audio_client, &mut default_period, &mut min_period);
            }
            let period = if min_period > 0 {
                min_period
            } else {
                default_period
            };

            // 7. Initialize in exclusive mode with event callback
            {
                type InitializeFn = unsafe extern "system" fn(
                    *mut c_void,
                    u32,
                    u32,
                    REFERENCE_TIME,
                    REFERENCE_TIME,
                    *const WAVEFORMATEX,
                    *const GUID,
                ) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let initialize: InitializeFn = std::mem::transmute(*vtable.add(3));
                let hr = initialize(
                    audio_client,
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    period,
                    period,
                    &wfx.Format as *const WAVEFORMATEX,
                    ptr::null(),
                );
                if hr != S_OK {
                    release(audio_client);
                    return Err(format!(
                        "IAudioClient::Initialize(EXCLUSIVE) failed: 0x{hr:08X}"
                    ));
                }
            }

            // 8. Create event and set it
            let event = CreateEventW(ptr::null(), 0, 0, ptr::null());
            if event.is_null() {
                release(audio_client);
                return Err("CreateEvent failed".into());
            }
            {
                type SetEventFn = unsafe extern "system" fn(*mut c_void, HANDLE) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let set_event: SetEventFn = std::mem::transmute(*vtable.add(13));
                let hr = set_event(audio_client, event);
                if hr != S_OK {
                    CloseHandle(event);
                    release(audio_client);
                    return Err(format!("SetEventHandle failed: 0x{hr:08X}"));
                }
            }

            // 9. Get buffer size
            let mut buffer_frame_count: u32 = 0;
            {
                type GetBufferSizeFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let get_size: GetBufferSizeFn = std::mem::transmute(*vtable.add(4));
                get_size(audio_client, &mut buffer_frame_count);
            }

            // 10. Get render client
            let mut render_client: *mut c_void = ptr::null_mut();
            {
                type GetServiceFn = unsafe extern "system" fn(
                    *mut c_void,
                    *const GUID,
                    *mut *mut c_void,
                ) -> HRESULT;
                let vtable = *(audio_client as *const *const *const c_void);
                let get_service: GetServiceFn = std::mem::transmute(*vtable.add(14));
                let hr = get_service(audio_client, &IID_IAUDIO_RENDER_CLIENT, &mut render_client);
                if hr != S_OK {
                    CloseHandle(event);
                    release(audio_client);
                    return Err(format!("GetService(IAudioRenderClient) failed: 0x{hr:08X}"));
                }
            }

            info!(
                device = %device_name,
                sample_rate,
                bit_depth,
                channels,
                buffer_frames = buffer_frame_count,
                period_100ns = period,
                "wasapi_exclusive_initialized"
            );

            Ok(Self {
                device_name: device_name.to_string(),
                sample_rate,
                bit_depth,
                channels,
                ring,
                volume,
                paused,
                audio_client,
                render_client,
                event_handle: event,
                buffer_frame_count,
                running: Arc::new(AtomicBool::new(false)),
                render_thread: None,
            })
        }
    }

    /// Start the render thread that feeds audio from the ring buffer
    /// to the WASAPI exclusive output.
    #[cfg(target_os = "windows")]
    pub fn start(&mut self) -> Result<(), String> {
        use ffi::*;

        self.running.store(true, Ordering::SeqCst);

        // Start the audio client
        unsafe {
            type StartFn = unsafe extern "system" fn(*mut std::ffi::c_void) -> HRESULT;
            let vtable = *(self.audio_client as *const *const *const std::ffi::c_void);
            let start: StartFn = std::mem::transmute(*vtable.add(10));
            let hr = start(self.audio_client);
            if hr != S_OK {
                return Err(format!("IAudioClient::Start failed: 0x{hr:08X}"));
            }
        }

        let ring = self.ring.clone();
        let volume = self.volume.clone();
        let paused = self.paused.clone();
        let running = self.running.clone();
        let render_client = self.render_client as usize; // Send as usize (pointer)
        let event_handle = self.event_handle as usize;
        let buffer_frame_count = self.buffer_frame_count;
        let channels = self.channels;
        let bytes_per_sample = self.bit_depth / 8;
        let frame_bytes = channels * bytes_per_sample;

        let handle = std::thread::spawn(move || {
            const S_OK_LOCAL: i32 = 0;
            const WAIT_OBJECT_0_LOCAL: u32 = 0;

            unsafe extern "system" {
                fn WaitForSingleObject(h: *mut std::ffi::c_void, ms: u32) -> u32;
            }

            let render_client = render_client as *mut std::ffi::c_void;
            let event_handle = event_handle as *mut std::ffi::c_void;

            info!("wasapi_exclusive_render_thread_started");

            while running.load(Ordering::SeqCst) {
                let wait_result = unsafe { WaitForSingleObject(event_handle, 2000) };
                if wait_result != WAIT_OBJECT_0_LOCAL {
                    if running.load(Ordering::SeqCst) {
                        debug!("wasapi_exclusive_wait_timeout");
                    }
                    continue;
                }

                if paused.load(Ordering::SeqCst) {
                    unsafe {
                        let mut buf: *mut u8 = std::ptr::null_mut();
                        type GetBufferFn = unsafe extern "system" fn(
                            *mut std::ffi::c_void,
                            u32,
                            *mut *mut u8,
                        )
                            -> i32;
                        let vtable = *(render_client as *const *const *const std::ffi::c_void);
                        let get_buf: GetBufferFn = std::mem::transmute(*vtable.add(3));
                        if get_buf(render_client, buffer_frame_count, &mut buf) == S_OK_LOCAL {
                            std::ptr::write_bytes(
                                buf,
                                0,
                                (buffer_frame_count * frame_bytes) as usize,
                            );
                            type RelBufFn =
                                unsafe extern "system" fn(*mut std::ffi::c_void, u32, u32) -> i32;
                            let rel_buf: RelBufFn = std::mem::transmute(*vtable.add(4));
                            rel_buf(render_client, buffer_frame_count, 0);
                        }
                    }
                    continue;
                }

                let needed_samples = (buffer_frame_count * channels) as usize;
                unsafe {
                    let mut buf: *mut u8 = std::ptr::null_mut();
                    type GetBufferFn =
                        unsafe extern "system" fn(*mut std::ffi::c_void, u32, *mut *mut u8) -> i32;
                    let vtable = *(render_client as *const *const *const std::ffi::c_void);
                    let get_buf: GetBufferFn = std::mem::transmute(*vtable.add(3));
                    let hr = get_buf(render_client, buffer_frame_count, &mut buf);
                    if hr != S_OK_LOCAL {
                        debug!(
                            hr = format!("0x{hr:08X}"),
                            "wasapi_exclusive_getbuffer_failed"
                        );
                        continue;
                    }

                    // Read from ring buffer (f32 samples)
                    let mut samples = vec![0.0f32; needed_samples];
                    let read = ring.pop(&mut samples);

                    // Apply volume
                    let vol = volume.load(Ordering::Relaxed) as f32 / 1000.0;
                    if vol < 0.999 {
                        for s in &mut samples[..read] {
                            *s *= vol;
                        }
                    }
                    // Zero any unread samples
                    for s in &mut samples[read..] {
                        *s = 0.0;
                    }

                    // Convert f32 to the target bit depth and write to WASAPI buffer
                    let out_slice = std::slice::from_raw_parts_mut(
                        buf,
                        (buffer_frame_count * frame_bytes) as usize,
                    );
                    match bytes_per_sample {
                        2 => {
                            for (i, &s) in samples.iter().enumerate() {
                                let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                                let bytes = val.to_le_bytes();
                                let off = i * 2;
                                if off + 1 < out_slice.len() {
                                    out_slice[off] = bytes[0];
                                    out_slice[off + 1] = bytes[1];
                                }
                            }
                        }
                        3 => {
                            for (i, &s) in samples.iter().enumerate() {
                                let val = (s.clamp(-1.0, 1.0) * 8388607.0) as i32;
                                let bytes = val.to_le_bytes();
                                let off = i * 3;
                                if off + 2 < out_slice.len() {
                                    out_slice[off] = bytes[0];
                                    out_slice[off + 1] = bytes[1];
                                    out_slice[off + 2] = bytes[2];
                                }
                            }
                        }
                        4 => {
                            for (i, &s) in samples.iter().enumerate() {
                                let val = (s.clamp(-1.0, 1.0) * 2147483647.0) as i32;
                                let bytes = val.to_le_bytes();
                                let off = i * 4;
                                if off + 3 < out_slice.len() {
                                    out_slice[off] = bytes[0];
                                    out_slice[off + 1] = bytes[1];
                                    out_slice[off + 2] = bytes[2];
                                    out_slice[off + 3] = bytes[3];
                                }
                            }
                        }
                        _ => {
                            std::ptr::write_bytes(buf, 0, out_slice.len());
                        }
                    }

                    type RelBufFn =
                        unsafe extern "system" fn(*mut std::ffi::c_void, u32, u32) -> i32;
                    let rel_buf: RelBufFn = std::mem::transmute(*vtable.add(4));
                    rel_buf(render_client, buffer_frame_count, 0);
                }
            }

            info!("wasapi_exclusive_render_thread_stopped");
        });

        self.render_thread = Some(handle);
        info!(
            device = %self.device_name,
            sample_rate = self.sample_rate,
            bit_depth = self.bit_depth,
            "wasapi_exclusive_started"
        );
        Ok(())
    }

    /// Stop and release all WASAPI resources.
    #[cfg(target_os = "windows")]
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Stop the audio client
        unsafe {
            type StopFn = unsafe extern "system" fn(*mut std::ffi::c_void) -> i32;
            let vtable = *(self.audio_client as *const *const *const std::ffi::c_void);
            let stop: StopFn = std::mem::transmute(*vtable.add(11));
            stop(self.audio_client);
        }

        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }

        unsafe {
            ffi::release(self.render_client);
            ffi::release(self.audio_client);
            ffi::CloseHandle(self.event_handle);
        }

        info!(device = %self.device_name, "wasapi_exclusive_stopped");
    }

    pub fn format_info(&self) -> String {
        format!(
            "WASAPI Exclusive {}ch {}bit {}Hz (buffer: {} frames)",
            self.channels, self.bit_depth, self.sample_rate, self.buffer_frame_count
        )
    }
}

#[cfg(target_os = "windows")]
impl Drop for WasapiExclusiveOutput {
    fn drop(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}

// Stub for non-Windows platforms
#[cfg(not(target_os = "windows"))]
impl WasapiExclusiveOutput {
    pub fn new(
        _device_name: &str,
        _sample_rate: u32,
        _bit_depth: u32,
        _channels: u32,
        _ring: Arc<RingBuf>,
        _volume: Arc<AtomicU32>,
        _paused: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Err("WASAPI Exclusive is only available on Windows".into())
    }
}

/// Returns true if WASAPI Exclusive mode is available on this platform.
pub fn supports_wasapi_exclusive() -> bool {
    cfg!(target_os = "windows")
}
