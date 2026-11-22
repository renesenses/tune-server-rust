//! Native rate-changing streams; all storage is supplied by the caller.
use super::*;
use tune_plugin_sdk::rate::{RateReport, RateRequest, check_report};

fn bytes(block: &mut AudioBlock<'_>) -> abi::Buffer {
    fn slice<T>(s: &mut [T]) -> abi::Buffer {
        abi::Buffer {
            data: s.as_mut_ptr().cast(),
            len: size_of_val(s) as u64,
        }
    }
    match block.samples_mut() {
        SamplesMut::S16(s) => slice(s),
        SamplesMut::S24Le(s) => slice(s),
        SamplesMut::S32(s) => slice(s),
        SamplesMut::F32(s) => slice(s),
        SamplesMut::F64(s) => slice(s),
    }
}

impl Library {
    pub fn negotiate_rate(
        &self,
        format: AudioFormat,
        accepted: &[u32],
        settings: &Settings,
    ) -> Result<Option<u32>, Error> {
        if self.api().kind != abi::rate::KIND {
            return Err(Error::CapabilityMissing);
        }
        let mut json = serde_json::to_vec(&abi::rate::Negotiation {
            accepted: accepted.to_vec(),
            settings: settings.clone(),
        })
        .map_err(|_| Error::InvalidSettings)?;
        let mut r = abi::Request::new(abi::rate::NEGOTIATE);
        r.format = abi::Format::from_audio(format);
        r.data = abi::Buffer {
            data: json.as_mut_ptr(),
            len: json.len() as u64,
        };
        self.call(&mut r)?;
        let rate = u32::try_from(r.other).map_err(|_| Error::InvalidFormat)?;
        if rate > 0 && !accepted.contains(&rate) {
            return Err(Error::InvalidFormat);
        }
        Ok((rate != 0).then_some(rate))
    }

    pub fn prepare_rate(
        self: &Arc<Self>,
        request: RateRequest,
        settings: &Settings,
    ) -> Result<NativeRate, Error> {
        if self.api().kind != abi::rate::KIND {
            return Err(Error::CapabilityMissing);
        }
        let request =
            RateRequest::new(request.input, request.output_rate, request.max_input_frames)?;
        let mut r = abi::Request::new(abi::CREATE);
        r.format = abi::Format::from_audio(request.input);
        r.other = u64::from(request.output_rate);
        r.frames = request.max_input_frames as u32;
        let mut json = serde_json::to_vec(settings).map_err(|_| Error::InvalidSettings)?;
        r.data = abi::Buffer {
            data: json.as_mut_ptr(),
            len: json.len() as u64,
        };
        self.call(&mut r)?;
        if r.handle == 0 {
            return Err(Error::InvalidState);
        }
        let mut stream = NativeRate {
            library: self.clone(),
            handle: r.handle,
            request,
            latency: 0,
            consumed: 0,
            written: 0,
        };
        stream.latency = u32::try_from(r.other).map_err(|_| Error::InvalidState)?;
        Ok(stream)
    }
}

pub struct NativeRate {
    library: Arc<Library>,
    handle: u64,
    request: RateRequest,
    latency: u32,
    consumed: u64,
    written: u64,
}
impl NativeRate {
    pub fn latency_frames(&self) -> u32 {
        self.latency
    }
    pub fn process(
        &mut self,
        input: &mut AudioBlock<'_>,
        output: &mut AudioBlock<'_>,
        context: BlockContext,
    ) -> Result<RateReport, Error> {
        if input.format() != self.request.input || output.format() != self.request.output_format() {
            return Err(Error::InvalidFormat);
        }
        if input.frames() > self.request.max_input_frames
            || output.frames() != self.request.max_output_frames()
        {
            return Err(Error::BlockTooLarge);
        }
        let mut r = abi::Request::new(abi::PROCESS);
        r.handle = self.handle;
        r.data = bytes(input);
        r.reply = bytes(output);
        r.zone_id = context.zone_id;
        r.generation = context.generation;
        r.position_frames = context.position_frames;
        self.library.call(&mut r)?;
        let report = RateReport {
            frames_consumed: r.frames as usize,
            frames_written: usize::try_from(r.other).map_err(|_| Error::InvalidState)?,
            clipped_samples: r.clipped,
            non_finite_samples: r.non_finite,
        };
        check_report(
            &self.request,
            input.frames(),
            &report,
            self.consumed,
            self.written,
        )?;
        self.consumed += report.frames_consumed as u64;
        self.written += report.frames_written as u64;
        Ok(report)
    }
    pub fn drain(&mut self, output: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        if output.format() != self.request.output_format() {
            return Err(Error::InvalidFormat);
        }
        let mut r = abi::Request::new(abi::DRAIN);
        r.handle = self.handle;
        r.reply = bytes(output);
        self.library.call(&mut r)?;
        if r.other > output.frames() as u64 {
            return Err(Error::InvalidState);
        }
        Ok(DrainReport {
            frames_written: r.other as usize,
            complete: r.flags != 0,
        })
    }
    pub fn reset(&mut self, reason: ResetReason) -> Result<(), Error> {
        let mut r = abi::Request::new(abi::RESET);
        r.handle = self.handle;
        r.other = match reason {
            ResetReason::NewTrack => 0,
            ResetReason::Seek => 1,
            ResetReason::FormatChange => 2,
            ResetReason::Stop => 3,
        };
        self.library.call(&mut r)?;
        self.consumed = 0;
        self.written = 0;
        Ok(())
    }
}
impl Drop for NativeRate {
    fn drop(&mut self) {
        let mut r = abi::Request::new(abi::DESTROY);
        r.handle = self.handle;
        let _ = self.library.call(&mut r);
    }
}
