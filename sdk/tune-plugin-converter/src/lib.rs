//! Batch conversion. The host owns codec discovery, resampling, quantization,
//! metadata, authorized sources and atomic publication; plugins own job policy.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::{Error, Settings, batch::*};

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub format: String,
    pub quality: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    #[serde(default = "download")]
    pub destination: Destination,
}
fn download() -> Destination {
    Destination::Download
}
pub struct Converter;
impl BatchTool for Converter {
    fn run(
        &self,
        host: &mut dyn BatchHost,
        sources: &[SourceSelection],
        options: &Settings,
    ) -> Result<JobResult, Error> {
        let options: Options =
            serde_json::from_value(options.clone()).map_err(|_| Error::InvalidSettings)?;
        if options.format.is_empty()
            || options.sample_rate == Some(0)
            || options
                .bit_depth
                .is_some_and(|d| ![16, 24, 32].contains(&d))
        {
            return Err(Error::InvalidSettings);
        }
        let sources = host.resolve(sources)?;
        let mut result = JobResult {
            state: JobState::Completed,
            artifacts: vec![],
            failures: vec![],
        };
        for (index, source) in sources.iter().enumerate() {
            if host.cancelled() {
                result.state = JobState::Cancelled;
                break;
            }
            let converted = convert(host, source, &options);
            match converted {
                Ok(artifact) => result.artifacts.push(artifact),
                Err(Error::Cancelled) => {
                    result.state = JobState::Cancelled;
                    break;
                }
                Err(error) => result.failures.push(FileFailure {
                    source: source.clone(),
                    code: error.to_string(),
                }),
            }
            host.progress(index + 1, sources.len(), source);
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
fn convert(
    host: &mut dyn BatchHost,
    source: &SourceHandle,
    options: &Options,
) -> Result<ArtifactHandle, Error> {
    // Probe is independent of decoding: hosts may use an external codec without
    // first decoding an entire track merely to discover its rate/depth.
    let format = host.source_format(source)?;
    let options_out = EncodeOptions {
        codec: options.format.clone(),
        sample_rate: options.sample_rate.unwrap_or(format.sample_rate()),
        bit_depth: options.bit_depth.unwrap_or(bit_depth(format.encoding())?),
        quality: options.quality.clone(),
    };
    let output = host.create_output(source, &options_out, &options.destination)?;
    let result = (|| {
        host.render_source(source, &output, &options_out)?;
        if host.cancelled() {
            return Err(Error::Cancelled);
        }
        host.copy_metadata(source, &output)?;
        host.finish(&output)
    })();
    if result.is_err() {
        host.abort(&output);
    }
    result
}
pub fn bit_depth(encoding: tune_plugin_sdk::audio::SampleEncoding) -> Result<u16, Error> {
    use tune_plugin_sdk::audio::SampleEncoding::*;
    match encoding {
        S16 => Ok(16),
        S24Le => Ok(24),
        S32 => Ok(32),
        _ => Err(Error::UnsupportedFormat),
    }
}

#[cfg(feature = "native")]
tune_plugin_abi::export_batch!(crate::Converter, include_str!("../manifest.json"));
