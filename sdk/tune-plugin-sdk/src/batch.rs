//! Host-managed jobs and file capabilities. Handles designate authorized
//! resources, never arbitrary paths supplied by a plugin. All calls here are
//! control/worker-thread operations, never real-time audio callbacks.
use crate::{Error, Settings, audio::AudioFormat};
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SourceSelection {
    Track(i64),
    Album(i64),
    DirectoryGrant(String),
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHandle(pub String);
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterHandle(pub String);
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactHandle(pub String);

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeOptions {
    pub codec: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub quality: Option<String>,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCapability {
    pub codec: String,
    pub sample_rates: Vec<u32>,
    pub bit_depths: Vec<u16>,
    /// Empty = codec has no named quality option; `None` always means default.
    pub qualities: Vec<String>,
}

impl CodecCapability {
    pub fn supports(&self, options: &EncodeOptions) -> bool {
        self.codec == options.codec
            && self.sample_rates.contains(&options.sample_rate)
            && self.bit_depths.contains(&options.bit_depth)
            && options
                .quality
                .as_ref()
                .is_none_or(|q| self.qualities.contains(q))
    }
}

/// Interleaved signed integers, right-justified at the format's bit depth.
/// The reader reports complete FRAMES, not samples. Caller owns the buffer.
/// Implementations reject buffers too small for one complete frame.
pub trait PcmReader: Send {
    fn format(&self) -> AudioFormat;
    fn read_frames(&mut self, samples: &mut [i32]) -> Result<usize, Error>;
    /// Required for a non-causal file tool; unsupported sources return an error.
    fn seek_frame(&mut self, frame: u64) -> Result<(), Error>;
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "grant", rename_all = "snake_case")]
pub enum Destination {
    Download,
    DirectoryGrant(String),
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Completed,
    Partial,
    Failed,
    Cancelled,
    Interrupted,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFailure {
    pub source: SourceHandle,
    pub code: String,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub state: JobState,
    pub artifacts: Vec<ArtifactHandle>,
    pub failures: Vec<FileFailure>,
}

/// Neither the plugin nor the UI creates grants. Production hosts MUST scope
/// handles to the current plugin/job and authorize every use. These Rust
/// newtypes are identifiers, not a security boundary against native code.
pub trait BatchHost: Send {
    fn resolve(&mut self, selections: &[SourceSelection]) -> Result<Vec<SourceHandle>, Error>;
    fn codecs(&self) -> Vec<CodecCapability>;
    fn cancelled(&self) -> bool;
    fn open(&mut self, source: &SourceHandle) -> Result<Box<dyn PcmReader>, Error>;
    /// Cheap format probe if available. Default opens a reader. Must describe
    /// decoded integer PCM, never the carrier of DSD/DoP.
    fn source_format(&mut self, source: &SourceHandle) -> Result<AudioFormat, Error> {
        Ok(self.open(source)?.format())
    }
    /// Convert through host codecs, including resampling/depth/quality. Default
    /// streams equal-format PCM; a host supporting conversion overrides this.
    /// Check cancellation between blocks. Never publish here: finish owns it.
    fn render_source(
        &mut self,
        source: &SourceHandle,
        output: &WriterHandle,
        options: &EncodeOptions,
    ) -> Result<(), Error> {
        let mut reader = self.open(source)?;
        let format = reader.format();
        let depth = match format.encoding() {
            crate::audio::SampleEncoding::S16 => 16,
            crate::audio::SampleEncoding::S24Le => 24,
            crate::audio::SampleEncoding::S32 => 32,
            _ => return Err(Error::UnsupportedFormat),
        };
        if options.sample_rate != format.sample_rate() || options.bit_depth != depth {
            return Err(Error::CapabilityMissing);
        }
        let mut samples = vec![0; 4096 * usize::from(format.channels())];
        loop {
            if self.cancelled() {
                return Err(Error::Cancelled);
            }
            let count = reader.read_frames(&mut samples)?;
            if count == 0 {
                break;
            }
            let count = count
                .checked_mul(usize::from(format.channels()))
                .filter(|n| *n <= samples.len())
                .ok_or(Error::HostFailure)?;
            self.write_frames(output, &samples[..count])?;
        }
        Ok(())
    }
    fn create_output(
        &mut self,
        source: &SourceHandle,
        options: &EncodeOptions,
        destination: &Destination,
    ) -> Result<WriterHandle, Error>;
    fn write_frames(&mut self, output: &WriterHandle, samples: &[i32]) -> Result<(), Error>;
    fn copy_metadata(&mut self, source: &SourceHandle, output: &WriterHandle) -> Result<(), Error>;
    fn finish(&mut self, output: &WriterHandle) -> Result<ArtifactHandle, Error>;
    /// Abort only this job's unpublished output. Preserve the source and any
    /// pre-existing destination. The host also cleans orphaned outputs on error.
    fn abort(&mut self, output: &WriterHandle);
    fn progress(&mut self, completed: usize, total: usize, current: &SourceHandle);
}

pub trait BatchTool: Send + Sync {
    fn run(
        &self,
        host: &mut dyn BatchHost,
        sources: &[SourceSelection],
        options: &Settings,
    ) -> Result<JobResult, Error>;
}
