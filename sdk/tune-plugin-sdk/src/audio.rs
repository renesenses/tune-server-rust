//! PCM processing contracts. Frame counts always include all channels.
//! A bypass is performed by the host before any sample conversion.
use crate::{Error, Settings};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleEncoding {
    S16,
    S24Le,
    S32,
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    Mono,
    Stereo,
    /// Channel order must be agreed separately; never infer stereo pairs.
    Discrete(u16),
}

impl ChannelLayout {
    pub fn channels(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Discrete(n) => n,
        }
    }
}

/// Construct with `new`; private fields prevent an invalid format reaching a
/// processor. Wire formats must be validated through the same constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    sample_rate: u32,
    layout: ChannelLayout,
    encoding: SampleEncoding,
}

impl AudioFormat {
    pub fn new(
        sample_rate: u32,
        layout: ChannelLayout,
        encoding: SampleEncoding,
    ) -> Result<Self, Error> {
        if !(1..=768_000).contains(&sample_rate) || !(1..=64).contains(&layout.channels()) {
            return Err(Error::InvalidFormat);
        }
        Ok(Self {
            sample_rate,
            layout,
            encoding,
        })
    }
    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }
    pub fn layout(self) -> ChannelLayout {
        self.layout
    }
    pub fn channels(self) -> u16 {
        self.layout.channels()
    }
    pub fn encoding(self) -> SampleEncoding {
        self.encoding
    }
}

/// Borrowed, interleaved PCM. S24 is packed signed little endian; other integer
/// variants use native Rust integers. DSD and DoP are intentionally absent.
pub enum SamplesMut<'a> {
    S16(&'a mut [i16]),
    S24Le(&'a mut [u8]),
    S32(&'a mut [i32]),
    F32(&'a mut [f32]),
    F64(&'a mut [f64]),
}

pub struct AudioBlock<'a> {
    format: AudioFormat,
    frames: usize,
    samples: SamplesMut<'a>,
}

impl<'a> AudioBlock<'a> {
    pub fn new(
        format: AudioFormat,
        samples: SamplesMut<'a>,
        max_frames: usize,
    ) -> Result<Self, Error> {
        let (encoding, count) = match &samples {
            SamplesMut::S16(s) => (SampleEncoding::S16, s.len()),
            SamplesMut::S24Le(s) => {
                if !s.len().is_multiple_of(3) {
                    return Err(Error::IncompleteFrame);
                }
                (SampleEncoding::S24Le, s.len() / 3)
            }
            SamplesMut::S32(s) => (SampleEncoding::S32, s.len()),
            SamplesMut::F32(s) => (SampleEncoding::F32, s.len()),
            SamplesMut::F64(s) => (SampleEncoding::F64, s.len()),
        };
        if encoding != format.encoding {
            return Err(Error::InvalidFormat);
        }
        if !count.is_multiple_of(usize::from(format.channels())) {
            return Err(Error::IncompleteFrame);
        }
        let frames = count / usize::from(format.channels());
        if max_frames == 0 || frames > max_frames {
            return Err(Error::BlockTooLarge);
        }
        Ok(Self {
            format,
            frames,
            samples,
        })
    }
    pub fn format(&self) -> AudioFormat {
        self.format
    }
    pub fn frames(&self) -> usize {
        self.frames
    }
    pub fn samples_mut(&mut self) -> SamplesMut<'_> {
        // Reborrow the payload instead of exposing the enum slot: callers
        // cannot replace it with a different format or frame count.
        match &mut self.samples {
            SamplesMut::S16(s) => SamplesMut::S16(s),
            SamplesMut::S24Le(s) => SamplesMut::S24Le(s),
            SamplesMut::S32(s) => SamplesMut::S32(s),
            SamplesMut::F32(s) => SamplesMut::F32(s),
            SamplesMut::F64(s) => SamplesMut::F64(s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Library,
    Radio,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Local,
    NetworkProgressive,
    NetworkFile,
    Browser,
    Other,
}

/// Facts provided BEFORE resolution. Applicability depends on the source as
/// well as the output: a streaming pretranscode and a library-file transcode
/// must not be conflated merely because both deliver a file to a renderer.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackContext {
    pub zone_id: i64,
    pub source: SourceKind,
    pub delivery: Delivery,
    pub pure: bool,
    pub protected_bitstream: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassReason {
    Disabled,
    Neutral,
    Pure,
    ProtectedBitstream,
    Unlicensed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applicability {
    Process { requires_pcm: bool },
    Bypass(BypassReason),
    Unsupported { reason: &'static str },
}

/// Host-owned policy, evaluated even when a plugin claims to support a stream.
pub fn policy_bypass(ctx: &PlaybackContext, licensed: bool) -> Option<BypassReason> {
    if ctx.protected_bitstream {
        Some(BypassReason::ProtectedBitstream)
    } else if ctx.pure {
        Some(BypassReason::Pure)
    } else if !licensed {
        Some(BypassReason::Unlicensed)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    NewTrack,
    Seek,
    FormatChange,
    Stop,
}

#[derive(Debug, Clone, Copy)]
pub struct BlockContext {
    pub zone_id: i64,
    pub generation: u64,
    pub position_frames: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessReport {
    pub changed: bool,
    pub clipped_samples: u64,
    pub non_finite_samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub frames_written: usize,
    pub complete: bool,
}

/// A processor has one owner and belongs to one stream. `reset` is NOT called
/// on every block or every parameter update. Hot updates are prepared off the
/// audio thread as a new processor and committed by a host with an explicit
/// transition policy. There is no generic hot-swap promise in this version.
pub trait Processor: Send {
    fn process(
        &mut self,
        block: &mut AudioBlock<'_>,
        context: BlockContext,
    ) -> Result<ProcessReport, Error>;
    fn reset(&mut self, reason: ResetReason);
    fn latency_frames(&self) -> u32;
    /// Output capacity is `block.frames()`. Only `frames_written` are valid.
    fn drain(&mut self, block: &mut AudioBlock<'_>) -> Result<DrainReport, Error>;
}

/// JSON settings only cross the control plane. `prepare` allocates all state;
/// host buffers have at most `max_frames` complete frames per call.
pub trait DspFactory: Send + Sync {
    fn assess(
        &self,
        context: &PlaybackContext,
        settings: &Settings,
    ) -> Result<Applicability, Error>;
    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        settings: &Settings,
    ) -> Result<Box<dyn Processor>, Error>;
}

/// Requested settings and effective audio state are deliberately separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApplicationState {
    Saved { requested_revision: u64 },
    AppliedLive { effective_revision: u64 },
    PendingNextTrack { requested_revision: u64 },
    PendingRestart { requested_revision: u64 },
    Bypassed { reason: BypassReason },
    Unsupported { reason: String },
}
