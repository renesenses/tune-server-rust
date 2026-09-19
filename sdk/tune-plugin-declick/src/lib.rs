//! Dé-ploc: trim silence and snap channel-0 edges to zero crossings. Does not
//! claim to repair arbitrary clicks within a recording. PCM remains integer.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::{Error, Settings, audio::SampleEncoding, batch::*};
mod engine;
pub use engine::trim_window;
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct TrimOptions {
    pub threshold_db: f32,
    pub trim_lead: bool,
    pub trim_tail: bool,
    pub zero_cross: bool,
}
impl Default for TrimOptions {
    fn default() -> Self {
        Self {
            threshold_db: -60.0,
            trim_lead: true,
            trim_tail: true,
            zero_cross: true,
        }
    }
}
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    #[serde(flatten)]
    pub trim: TrimOptions,
    pub output_format: String,
    pub destination: Destination,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            trim: TrimOptions::default(),
            output_format: "flac".into(),
            destination: Destination::Download,
        }
    }
}
pub struct Declick;
impl BatchTool for Declick {
    fn run(
        &self,
        host: &mut dyn BatchHost,
        selections: &[SourceSelection],
        settings: &Settings,
    ) -> Result<JobResult, Error> {
        let options: Options =
            serde_json::from_value(settings.clone()).map_err(|_| Error::InvalidSettings)?;
        if !options.trim.threshold_db.is_finite()
            || options.trim.threshold_db > 0.0
            || options.trim.threshold_db < -160.0
            || !["flac", "wav", "pcm-test"].contains(&options.output_format.as_str())
        {
            return Err(Error::InvalidSettings);
        }
        let sources = host.resolve(selections)?;
        let mut result = JobResult {
            state: JobState::Completed,
            artifacts: vec![],
            failures: vec![],
        };
        for (i, source) in sources.iter().enumerate() {
            if host.cancelled() {
                result.state = JobState::Cancelled;
                break;
            }
            match process(host, source, &options) {
                Ok(a) => result.artifacts.push(a),
                Err(Error::Cancelled) => {
                    result.state = JobState::Cancelled;
                    break;
                }
                Err(e) => result.failures.push(FileFailure {
                    source: source.clone(),
                    code: e.to_string(),
                }),
            }
            host.progress(i + 1, sources.len(), source);
        }
        if result.state != JobState::Cancelled && !result.failures.is_empty() {
            result.state = if result.artifacts.is_empty() {
                JobState::Failed
            } else {
                JobState::Partial
            };
        }
        Ok(result)
    }
}
fn process(
    host: &mut dyn BatchHost,
    source: &SourceHandle,
    options: &Options,
) -> Result<ArtifactHandle, Error> {
    let mut reader = host.open(source)?;
    let format = reader.format();
    let depth = match format.encoding() {
        SampleEncoding::S16 => 16,
        SampleEncoding::S24Le => 24,
        SampleEncoding::S32 => 32,
        _ => return Err(Error::UnsupportedFormat),
    };
    let channels = usize::from(format.channels());
    let mut samples = Vec::new();
    let mut block = vec![0; 4096 * channels];
    loop {
        if host.cancelled() {
            return Err(Error::Cancelled);
        }
        let count = reader.read_frames(&mut block)?;
        if count == 0 {
            break;
        }
        let count = count
            .checked_mul(channels)
            .filter(|n| *n <= block.len())
            .ok_or(Error::HostFailure)?;
        // The host reader also enforces its resource quota. This cap prevents
        // malicious/unbounded sources from exhausting plugin worker memory.
        if samples.len().saturating_add(count) > 256 * 1024 * 1024 {
            return Err(Error::BlockTooLarge);
        }
        samples.extend_from_slice(&block[..count]);
    }
    let range = trim_window(&samples, channels, depth, options.trim).map_err(|message| {
        if message == "track is entirely below the silence threshold" {
            Error::SilenceOnly
        } else {
            Error::InvalidSettings
        }
    })?;
    let encode = EncodeOptions {
        codec: options.output_format.clone(),
        sample_rate: format.sample_rate(),
        bit_depth: depth,
        quality: None,
    };
    let output = host.create_output(source, &encode, &options.destination)?;
    let result = (|| {
        for block in samples[range].chunks(4096 * channels) {
            if host.cancelled() {
                return Err(Error::Cancelled);
            }
            host.write_frames(&output, block)?;
        }
        host.copy_metadata(source, &output)?;
        if host.cancelled() {
            return Err(Error::Cancelled);
        }
        host.finish(&output)
    })();
    if result.is_err() {
        host.abort(&output);
    }
    result
}

#[cfg(feature = "native")]
tune_plugin_abi::export_batch!(crate::Declick, include_str!("../manifest.json"));
