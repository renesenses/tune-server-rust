use crate::Library;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{Arc, Mutex},
};
use tune_plugin_abi::{self as abi, Buffer, Format, batch::HostApi};
use tune_plugin_sdk::{Error, Settings, batch::*};
struct Bridge<'a> {
    host: &'a mut dyn BatchHost,
    readers: BTreeMap<u64, Box<dyn PcmReader>>,
    next: u64,
}
fn parse<T: DeserializeOwned>(v: Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(|_| Error::InvalidSettings)
}
fn value<T: Serialize>(v: T) -> Result<Value, Error> {
    serde_json::to_value(v).map_err(|_| Error::HostFailure)
}
unsafe fn bridge<'a>(context: *mut c_void) -> Result<&'a Mutex<Bridge<'a>>, Error> {
    if context.is_null() {
        return Err(Error::InvalidState);
    }
    Ok(unsafe { &*(context as *const Mutex<Bridge<'a>>) })
}
unsafe extern "C" fn control(context: *mut c_void, input: Buffer, output: *mut Buffer) -> i32 {
    abi::boundary(|| {
        if output.is_null() {
            return Err(Error::InvalidState);
        }
        let (operation, args): (String, Value) = serde_json::from_slice(unsafe { input.bytes()? })
            .map_err(|_| Error::InvalidSettings)?;
        let mutex = unsafe { bridge(context)? };
        let mut b = mutex.lock().map_err(|_| Error::InvalidState)?;
        let result = match operation.as_str() {
            "resolve" => value(b.host.resolve(&parse::<Vec<SourceSelection>>(args)?)?)?,
            "codecs" => value(b.host.codecs())?,
            "cancelled" => json!(b.host.cancelled()),
            "format" => value(Format::from_audio(b.host.source_format(&parse(args)?)?))?,
            "open" => {
                if b.readers.len() >= 64 {
                    return Err(Error::BlockTooLarge);
                }
                let reader = b.host.open(&parse(args)?)?;
                let format = Format::from_audio(reader.format());
                b.next = b.next.checked_add(1).ok_or(Error::InvalidState)?;
                let id = b.next;
                b.readers.insert(id, reader);
                json!([id, format])
            }
            "create" => {
                let (source, options, destination): (SourceHandle, EncodeOptions, Destination) =
                    parse(args)?;
                value(b.host.create_output(&source, &options, &destination)?)?
            }
            "render" => {
                let (source, output, options): (SourceHandle, WriterHandle, EncodeOptions) =
                    parse(args)?;
                b.host.render_source(&source, &output, &options)?;
                Value::Null
            }
            "metadata" => {
                let (source, output): (SourceHandle, WriterHandle) = parse(args)?;
                b.host.copy_metadata(&source, &output)?;
                Value::Null
            }
            "finish" => value(b.host.finish(&parse(args)?)?)?,
            "abort" => {
                b.host.abort(&parse(args)?);
                Value::Null
            }
            "progress" => {
                let (completed, total, current): (usize, usize, SourceHandle) = parse(args)?;
                b.host.progress(completed, total, &current);
                Value::Null
            }
            _ => return Err(Error::CapabilityMissing),
        };
        unsafe {
            *output = Buffer::owned(serde_json::to_vec(&result).map_err(|_| Error::HostFailure)?)
        };
        Ok(())
    })
}
unsafe extern "C" fn read(
    context: *mut c_void,
    id: u64,
    samples: *mut i32,
    capacity: u64,
    frames: *mut u64,
) -> i32 {
    abi::boundary(|| {
        if samples.is_null()
            || frames.is_null()
            || capacity == 0
            || capacity > 4 * 1024 * 1024
            || !(samples as usize).is_multiple_of(std::mem::align_of::<i32>())
        {
            return Err(Error::InvalidSettings);
        }
        let mutex = unsafe { bridge(context)? };
        let mut b = mutex.lock().map_err(|_| Error::InvalidState)?;
        let reader = b.readers.get_mut(&id).ok_or(Error::InvalidState)?;
        let channels = usize::from(reader.format().channels());
        let count = reader
            .read_frames(unsafe { std::slice::from_raw_parts_mut(samples, capacity as usize) })?;
        if count
            .checked_mul(channels)
            .is_none_or(|n| n > capacity as usize)
        {
            return Err(Error::HostFailure);
        }
        unsafe { *frames = count as u64 };
        Ok(())
    })
}
unsafe extern "C" fn seek(context: *mut c_void, id: u64, frame: u64) -> i32 {
    abi::boundary(|| {
        let mutex = unsafe { bridge(context)? };
        let mut b = mutex.lock().map_err(|_| Error::InvalidState)?;
        b.readers
            .get_mut(&id)
            .ok_or(Error::InvalidState)?
            .seek_frame(frame)
    })
}
unsafe extern "C" fn close(context: *mut c_void, id: u64) {
    let _ = abi::boundary(|| {
        let mutex = unsafe { bridge(context)? };
        let mut b = mutex.lock().map_err(|_| Error::InvalidState)?;
        b.readers.remove(&id);
        Ok(())
    });
}
unsafe extern "C" fn write(
    context: *mut c_void,
    output: Buffer,
    samples: *const i32,
    count: u64,
) -> i32 {
    abi::boundary(|| {
        if samples.is_null()
            || count > 4 * 1024 * 1024
            || !(samples as usize).is_multiple_of(std::mem::align_of::<i32>())
        {
            return Err(Error::InvalidSettings);
        }
        let output: WriterHandle = serde_json::from_slice(unsafe { output.bytes()? })
            .map_err(|_| Error::InvalidSettings)?;
        let mutex = unsafe { bridge(context)? };
        let mut b = mutex.lock().map_err(|_| Error::InvalidState)?;
        b.host.write_frames(&output, unsafe {
            std::slice::from_raw_parts(samples, count as usize)
        })
    })
}
pub fn run(
    library: &Arc<Library>,
    host: &mut dyn BatchHost,
    sources: &[SourceSelection],
    settings: &Settings,
) -> Result<JobResult, Error> {
    if library.api().kind != abi::BATCH {
        return Err(Error::CapabilityMissing);
    }
    let mut bridge = Mutex::new(Bridge {
        host,
        readers: BTreeMap::new(),
        next: 0,
    });
    let mut api = HostApi {
        size: std::mem::size_of::<HostApi>() as u32,
        version: abi::ABI_VERSION,
        context: (&mut bridge as *mut Mutex<Bridge<'_>>).cast(),
        control,
        free_reply: abi::free,
        read,
        seek,
        close,
        write,
    };
    let mut input = serde_json::to_vec(&abi::batch::Run {
        sources: sources.to_vec(),
        settings: settings.clone(),
    })
    .map_err(|_| Error::InvalidSettings)?;
    let mut request = abi::Request::new(abi::RUN_BATCH);
    request.host = &mut api;
    request.data = Buffer {
        data: input.as_mut_ptr(),
        len: input.len() as u64,
    };
    library.call(&mut request)?;
    library.reply(request.reply)
}
