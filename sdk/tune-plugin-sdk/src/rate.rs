//! Sample-rate changing stages (#7614).
//!
//! [`crate::audio::Processor`] works in place at a constant format: a block
//! leaves with the frame count and sample rate it entered with. An upsampler
//! cannot honour that contract, so it gets a separate stage:
//!
//! - the host asks the factory which output rate it wants, given the rates the
//!   sink accepts ([`RateChangerFactory::negotiate`]); `None` means the stage
//!   has nothing useful to do for this stream and the host bypasses it;
//! - `prepare` allocates everything for one stream, input format and output
//!   rate fixed; the stage never changes channel layout or encoding;
//! - [`RateChanger::process`] reads one whole input block and writes into a
//!   separate output block whose capacity is at least
//!   [`RateChanger::max_output_frames`];
//! - the emitted stream is delayed by [`RateChanger::latency_frames`], counted
//!   in OUTPUT frames, and [`RateChanger::drain`] flushes the tail.
//!
//! Frame accounting is exact up to one frame: after `n` input frames, the
//! frames written by `process` add up to `floor(n · out / in)` within one
//! frame. [`expected_output_frames`] gives the reference count.
//!
//! The same real-time rules as `Processor` apply: `process` and `drain` must
//! not allocate, block, perform I/O, query licensing or call host services.
//! The PURE and protected-bitstream policy of [`crate::audio::policy_bypass`]
//! applies unchanged: a host never instantiates a rate changer under PURE.
use crate::audio::{
    Applicability, AudioBlock, AudioFormat, BlockContext, DrainReport, PlaybackContext, ResetReason,
};
use crate::{Error, Settings};

/// Capability id a manifest requests to be hosted as a rate-changing stage.
/// It is deliberately absent from
/// [`crate::manifest::reference_host_capabilities`] until a host adapter and
/// its matrix witness exist.
pub const AUDIO_RATE_CHANGE: &str = "audio-rate-change";

/// Largest ratio a stage may request: 44.1 kHz to 768 kHz is below ×18.
pub const MAX_RATIO: u32 = 32;

/// What the host fixes before `prepare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRequest {
    pub input: AudioFormat,
    pub output_rate: u32,
    /// Largest input block the host will pass to `process`.
    pub max_input_frames: usize,
}

impl RateRequest {
    /// Validates the request and builds the output format: same layout and
    /// encoding as the input, at `output_rate`.
    pub fn new(
        input: AudioFormat,
        output_rate: u32,
        max_input_frames: usize,
    ) -> Result<Self, Error> {
        let output = AudioFormat::new(output_rate, input.layout(), input.encoding())?;
        if max_input_frames == 0 || max_input_frames > 1 << 20 {
            return Err(Error::BlockTooLarge);
        }
        let (i, o) = (input.sample_rate(), output.sample_rate());
        if o > i.saturating_mul(MAX_RATIO) || i > o.saturating_mul(MAX_RATIO) {
            return Err(Error::UnsupportedFormat);
        }
        Ok(Self {
            input,
            output_rate,
            max_input_frames,
        })
    }
    pub fn output_format(&self) -> AudioFormat {
        AudioFormat::new(self.output_rate, self.input.layout(), self.input.encoding())
            .expect("validated by RateRequest::new")
    }
    /// Output capacity sufficient for any block of at most
    /// `max_input_frames`, including the one-frame phase carry.
    pub fn max_output_frames(&self) -> usize {
        max_output_frames(
            self.input.sample_rate(),
            self.output_rate,
            self.max_input_frames,
        )
    }
}

/// `floor(input_frames · output_rate / input_rate)`, computed without
/// overflow. The reference count of [`RateChanger::process`] after
/// `input_frames` frames of input since `prepare` or `reset`.
pub fn expected_output_frames(input_rate: u32, output_rate: u32, input_frames: u64) -> u64 {
    if input_rate == 0 {
        return 0;
    }
    ((u128::from(input_frames) * u128::from(output_rate)) / u128::from(input_rate)) as u64
}

/// Capacity needed for one block of `input_frames`: the exact ratio rounded
/// up, plus one frame of phase carry.
pub fn max_output_frames(input_rate: u32, output_rate: u32, input_frames: usize) -> usize {
    if input_rate == 0 {
        return 0;
    }
    let n = u128::from(input_frames as u64) * u128::from(output_rate);
    let d = u128::from(input_rate);
    (n.div_ceil(d) as usize).saturating_add(1)
}

/// Per-block accounting. `frames_consumed` is always the whole input block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RateReport {
    pub frames_consumed: usize,
    pub frames_written: usize,
    pub clipped_samples: u64,
    pub non_finite_samples: u64,
}

/// One stream, one owner, like [`crate::audio::Processor`].
pub trait RateChanger: Send + std::any::Any {
    fn request(&self) -> RateRequest;
    fn output_format(&self) -> AudioFormat {
        self.request().output_format()
    }
    fn max_output_frames(&self) -> usize {
        self.request().max_output_frames()
    }
    /// Reads all of `input` (format = request input, frames ≤
    /// `max_input_frames`) and writes `frames_written` frames at the start of
    /// `output` (format = output format, capacity ≥ `max_output_frames`).
    /// Refuses with [`Error::InvalidFormat`] or [`Error::BlockTooLarge`]
    /// without touching its state.
    fn process(
        &mut self,
        input: &AudioBlock<'_>,
        output: &mut AudioBlock<'_>,
        context: BlockContext,
    ) -> Result<RateReport, Error>;
    /// Control/producer-thread update; may allocate. Failure preserves the
    /// previous settings.
    fn update(&mut self, _settings: &Settings) -> Result<(), Error> {
        Err(Error::CapabilityMissing)
    }
    fn diagnostics(&self) -> Settings {
        Settings::Null
    }
    /// Clears filter history and phase: the next output frame is aligned on
    /// the next input frame again.
    fn reset(&mut self, reason: ResetReason);
    /// Delay of the emitted stream, in OUTPUT frames.
    fn latency_frames(&self) -> u32;
    /// Flushes the tail into `output` (capacity = `output.frames()`). Only
    /// `frames_written` are valid; `complete` ends the stream.
    fn drain(&mut self, output: &mut AudioBlock<'_>) -> Result<DrainReport, Error>;
}

/// Control-plane factory. JSON settings never cross the audio thread.
pub trait RateChangerFactory: Send + Sync {
    fn assess(
        &self,
        context: &PlaybackContext,
        settings: &Settings,
    ) -> Result<Applicability, Error>;
    /// The output rate wanted for `input`, chosen among `accepted_rates`
    /// (the sink's accepted rates, any order). `None`: nothing to do for this
    /// stream (for example no accepted rate above the source), the host
    /// bypasses the stage and keeps its own path.
    fn negotiate(
        &self,
        input: AudioFormat,
        accepted_rates: &[u32],
        settings: &Settings,
    ) -> Result<Option<u32>, Error>;
    /// Allocates all state for one stream.
    fn prepare(
        &self,
        request: RateRequest,
        settings: &Settings,
    ) -> Result<Box<dyn RateChanger>, Error>;
}

/// Checks a report against the request and the cumulative count, so hosts
/// and test kits enforce the same accounting.
pub fn check_report(
    request: &RateRequest,
    input_frames: usize,
    report: &RateReport,
    consumed_before: u64,
    written_before: u64,
) -> Result<(), Error> {
    if report.frames_consumed != input_frames || report.frames_written > request.max_output_frames()
    {
        return Err(Error::InvalidState);
    }
    let consumed = consumed_before + input_frames as u64;
    let written = written_before + report.frames_written as u64;
    let expected =
        expected_output_frames(request.input.sample_rate(), request.output_rate, consumed);
    if written.abs_diff(expected) > 1 {
        return Err(Error::InvalidState);
    }
    Ok(())
}
