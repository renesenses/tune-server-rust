//! Example: lossless PCM copy via host services. No filesystem or codec access
//! lives in this plugin. A production host must enforce per-job grants.
use tune_plugin_sdk::{Error, Settings, audio::SampleEncoding, batch::*};

pub struct Plugin;

fn copy_one(
    host: &mut dyn BatchHost,
    source: &SourceHandle,
    codec: &str,
) -> Result<ArtifactHandle, Error> {
    if host.cancelled() {
        return Err(Error::Cancelled);
    }
    let mut input = host.open(source)?;
    let format = input.format();
    let bit_depth = match format.encoding() {
        SampleEncoding::S16 => 16,
        SampleEncoding::S24Le => 24,
        SampleEncoding::S32 => 32,
        _ => return Err(Error::UnsupportedFormat),
    };
    let options = EncodeOptions {
        codec: codec.into(),
        sample_rate: format.sample_rate(),
        bit_depth,
        quality: None,
    };
    if !host.codecs().iter().any(|c| c.supports(&options)) {
        return Err(Error::CapabilityMissing);
    }
    let output = host.create_output(source, &options, &Destination::Download)?;
    let result = (|| {
        let channels = usize::from(format.channels());
        let mut samples = vec![0i32; 1024 * channels];
        loop {
            if host.cancelled() {
                return Err(Error::Cancelled);
            }
            let frames = input.read_frames(&mut samples)?;
            if frames > 1024 {
                return Err(Error::InvalidState);
            }
            if frames == 0 {
                break;
            }
            host.write_frames(&output, &samples[..frames * channels])?;
        }
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

impl BatchTool for Plugin {
    fn run(
        &self,
        host: &mut dyn BatchHost,
        selections: &[SourceSelection],
        options: &Settings,
    ) -> Result<JobResult, Error> {
        let codec = options
            .get("codec")
            .and_then(|v| v.as_str())
            .ok_or(Error::InvalidSettings)?;
        let sources = host.resolve(selections)?;
        if sources.is_empty() {
            return Err(Error::InvalidSettings);
        }
        let mut result = JobResult {
            state: JobState::Running,
            artifacts: vec![],
            failures: vec![],
        };
        for (index, source) in sources.iter().enumerate() {
            match copy_one(host, source, codec) {
                Ok(artifact) => result.artifacts.push(artifact),
                Err(Error::Cancelled) => {
                    result.state = JobState::Cancelled;
                    return Ok(result);
                }
                Err(error) => result.failures.push(FileFailure {
                    source: source.clone(),
                    code: error.to_string(),
                }),
            }
            host.progress(index + 1, sources.len(), source);
        }
        result.state = if result.failures.is_empty() {
            JobState::Completed
        } else if result.artifacts.is_empty() {
            JobState::Failed
        } else {
            JobState::Partial
        };
        Ok(result)
    }
}
