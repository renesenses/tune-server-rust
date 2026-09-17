//! Scoped batch host for the premium SDK. Route authorization resolves paths
//! before construction. Plugins see job-local handles, never paths. Outputs
//! stay private until encoding and metadata complete, then publish no-clobber.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tune_core::audio::decode::DecodedAudio;
use tune_plugin_sdk::{
    Error,
    audio::{AudioFormat, ChannelLayout, SampleEncoding},
    batch::*,
};

struct Reader {
    decoded: DecodedAudio,
    offset: usize,
    format: AudioFormat,
}
impl PcmReader for Reader {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn read_frames(&mut self, out: &mut [i32]) -> Result<usize, Error> {
        let ch = usize::from(self.format.channels());
        if out.len() < ch {
            return Err(Error::IncompleteFrame);
        }
        let n = (out.len() / ch * ch).min(self.decoded.samples_i32.len() - self.offset);
        out[..n].copy_from_slice(&self.decoded.samples_i32[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n / ch)
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), Error> {
        let offset = usize::try_from(frame)
            .ok()
            .and_then(|n| n.checked_mul(usize::from(self.format.channels())))
            .filter(|n| *n <= self.decoded.samples_i32.len())
            .ok_or(Error::InvalidSettings)?;
        self.offset = offset;
        Ok(())
    }
}
fn pcm_format(decoded: &DecodedAudio) -> Result<AudioFormat, Error> {
    let encoding = match decoded.bit_depth {
        16 => SampleEncoding::S16,
        24 => SampleEncoding::S24Le,
        32 => SampleEncoding::S32,
        _ => return Err(Error::UnsupportedFormat),
    };
    let channels = u16::try_from(decoded.channels).map_err(|_| Error::InvalidFormat)?;
    let layout = match channels {
        1 => ChannelLayout::Mono,
        2 => ChannelLayout::Stereo,
        n => ChannelLayout::Discrete(n),
    };
    AudioFormat::new(decoded.sample_rate, layout, encoding)
}
struct Output {
    // TempDir keeps partial files out of the downloadable job directory listing.
    staging: tempfile::TempDir,
    path: PathBuf,
    options: EncodeOptions,
    channels: u32,
    samples: Vec<i32>,
    rendered: bool,
    metadata: bool,
}
pub(super) struct FileHost {
    input: PathBuf,
    output: PathBuf,
    decoded: Option<DecodedAudio>,
    channels: Option<u32>,
    writers: HashMap<String, Output>,
    serial: u64,
    cancellation: Arc<AtomicBool>,
    pub error: Option<String>,
}
impl FileHost {
    pub fn new(input: &Path, output: &Path, cancellation: Arc<AtomicBool>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            decoded: None,
            channels: None,
            writers: HashMap::new(),
            serial: 0,
            cancellation,
            error: None,
        }
    }
    fn failure(&mut self, error: impl ToString) -> Error {
        self.error = Some(error.to_string());
        Error::HostFailure
    }
    fn source(&self, source: &SourceHandle) -> Result<(), Error> {
        if source.0 == "source:0" {
            Ok(())
        } else {
            Err(Error::InvalidState)
        }
    }
    fn decode(&mut self) -> Result<(), Error> {
        if self.decoded.is_none() {
            let input = self.input.to_str().ok_or(Error::InvalidSettings)?;
            let d =
                super::converter::decode_for_convert(input, None).map_err(|e| self.failure(e))?;
            pcm_format(&d)?;
            if d.samples_i32.len() > 256 * 1024 * 1024 {
                return Err(Error::BlockTooLarge);
            }
            self.channels = Some(d.channels);
            self.decoded = Some(d);
        }
        Ok(())
    }
    fn materialize(&mut self, output: &WriterHandle) -> Result<(), Error> {
        let o = self.writers.get_mut(&output.0).ok_or(Error::InvalidState)?;
        if o.rendered {
            return Ok(());
        }
        if self.cancellation.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let pcm = super::converter::convert_bit_depth(
            &o.samples,
            o.options.bit_depth,
            o.options.bit_depth,
        );
        let result = match o.options.codec.as_str() {
            "flac" => super::converter::encode_flac(
                &pcm,
                o.options.sample_rate,
                u32::from(o.options.bit_depth),
                o.channels,
            ),
            "wav" => super::converter::encode_wav(
                &pcm,
                o.options.sample_rate,
                u32::from(o.options.bit_depth),
                o.channels,
            ),
            _ => return Err(Error::UnsupportedFormat),
        };
        match result.and_then(|bytes| std::fs::write(&o.path, bytes).map_err(|e| e.to_string())) {
            Ok(()) => {
                o.rendered = true;
                o.samples.clear();
                Ok(())
            }
            Err(e) => Err(self.failure(e)),
        }
    }
}
impl BatchHost for FileHost {
    fn resolve(&mut self, selections: &[SourceSelection]) -> Result<Vec<SourceHandle>, Error> {
        if selections != [SourceSelection::Track(0)] {
            return Err(Error::InvalidState);
        }
        Ok(vec![SourceHandle("source:0".into())])
    }
    fn codecs(&self) -> Vec<CodecCapability> {
        let capabilities =
            tokio::runtime::Handle::current().block_on(super::converter::capabilities_payload(&[]));
        capabilities["formats"]
            .as_object()
            .into_iter()
            .flat_map(|formats| formats.iter())
            .filter(|(_, available)| available.as_bool() == Some(true))
            .map(|(codec, _)| CodecCapability {
                codec: codec.clone(),
                sample_rates: if codec == "opus" {
                    vec![48000]
                } else {
                    vec![
                        8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000,
                        176400, 192000, 352800, 384000,
                    ]
                },
                bit_depths: if ["opus", "aac", "mp3"].contains(&codec.as_str()) {
                    vec![16]
                } else {
                    vec![16, 24, 32]
                },
                qualities: if ["opus", "aac", "mp3"].contains(&codec.as_str()) {
                    [64, 96, 128, 160, 192, 256, 320]
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                } else {
                    vec![]
                },
            })
            .collect()
    }
    fn cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }
    fn source_format(&mut self, source: &SourceHandle) -> Result<AudioFormat, Error> {
        self.source(source)?;
        self.decode()?;
        pcm_format(self.decoded.as_ref().ok_or(Error::InvalidState)?)
    }
    fn open(&mut self, source: &SourceHandle) -> Result<Box<dyn PcmReader>, Error> {
        self.source(source)?;
        self.decode()?;
        let decoded = self.decoded.take().ok_or(Error::InvalidState)?;
        let format = pcm_format(&decoded)?;
        Ok(Box::new(Reader {
            decoded,
            offset: 0,
            format,
        }))
    }
    fn create_output(
        &mut self,
        source: &SourceHandle,
        options: &EncodeOptions,
        destination: &Destination,
    ) -> Result<WriterHandle, Error> {
        self.source(source)?;
        if *destination != Destination::Download {
            return Err(Error::InvalidState);
        } // granted final path is fixed by the route, not chosen by the plugin
        if !["flac", "wav", "opus", "alac", "aac", "mp3"].contains(&options.codec.as_str())
            || ![16, 24, 32].contains(&options.bit_depth)
            || !(8000..=768000).contains(&options.sample_rate)
        {
            return Err(Error::UnsupportedFormat);
        }
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        if self.output.exists() {
            return Err(self.failure("destination file already exists, left untouched"));
        }
        // Reader may have taken the decoded object. Probe channels without losing ownership.
        if self.channels.is_none() {
            self.decode()?;
        }
        let channels = self.channels.ok_or(Error::InvalidFormat)?;
        let parent = self.output.parent().ok_or(Error::InvalidSettings)?;
        let staging = tempfile::Builder::new()
            .prefix(".tune-plugin-")
            .tempdir_in(parent)
            .map_err(|e| self.failure(e))?;
        let path = staging.path().join(format!(
            "audio.{}",
            super::converter::output_extension(&options.codec)
        ));
        self.serial += 1;
        let key = format!("writer:{}", self.serial);
        self.writers.insert(
            key.clone(),
            Output {
                staging,
                path,
                options: options.clone(),
                channels,
                samples: vec![],
                rendered: false,
                metadata: false,
            },
        );
        Ok(WriterHandle(key))
    }
    fn render_source(
        &mut self,
        source: &SourceHandle,
        output: &WriterHandle,
        options: &EncodeOptions,
    ) -> Result<(), Error> {
        self.source(source)?;
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        let o = self.writers.get(&output.0).ok_or(Error::InvalidState)?;
        if o.options != *options {
            return Err(Error::InvalidSettings);
        }
        let result = tokio::runtime::Handle::current().block_on(super::converter::encode_source(
            &self.input,
            &o.path,
            &options.codec,
            options.quality.as_deref(),
            Some(options.sample_rate),
            Some(options.bit_depth),
        ));
        result.map_err(|e| self.failure(e))?;
        self.writers
            .get_mut(&output.0)
            .ok_or(Error::InvalidState)?
            .rendered = true;
        if self.cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    fn write_frames(&mut self, output: &WriterHandle, samples: &[i32]) -> Result<(), Error> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        let o = self.writers.get_mut(&output.0).ok_or(Error::InvalidState)?;
        if o.rendered || !samples.len().is_multiple_of(o.channels as usize) {
            return Err(Error::IncompleteFrame);
        }
        if o.samples.len().saturating_add(samples.len()) > 256 * 1024 * 1024 {
            return Err(Error::BlockTooLarge);
        }
        o.samples.extend_from_slice(samples);
        Ok(())
    }
    fn copy_metadata(&mut self, source: &SourceHandle, output: &WriterHandle) -> Result<(), Error> {
        self.source(source)?;
        self.materialize(output)?;
        let o = self.writers.get_mut(&output.0).ok_or(Error::InvalidState)?;
        // Preserve the existing best-effort metadata contract and surface the warning.
        if let Err(e) = super::converter::copy_tags(&self.input, &o.path) {
            tracing::warn!(error=%e, "plugin_copy_tags_failed");
        }
        o.metadata = true;
        Ok(())
    }
    fn finish(&mut self, output: &WriterHandle) -> Result<ArtifactHandle, Error> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        self.materialize(output)?;
        let o = self.writers.remove(&output.0).ok_or(Error::InvalidState)?;
        if !o.metadata {
            return Err(Error::InvalidState);
        }
        // Same filesystem: hard_link is an atomic, exclusive publication. An
        // existing destination (including a racing symlink) is never replaced.
        std::fs::hard_link(&o.path, &self.output).map_err(|e| self.failure(e))?;
        drop(o.staging);
        Ok(ArtifactHandle("artifact:0".into()))
    }
    fn abort(&mut self, output: &WriterHandle) {
        self.writers.remove(&output.0);
    }
    fn progress(&mut self, _: usize, _: usize, _: &SourceHandle) {}
}

pub(super) fn run_installed(
    id: &str,
    fallback: &dyn BatchTool,
    input: &Path,
    output: &Path,
    options: &serde_json::Value,
    cancellation: Arc<AtomicBool>,
) -> Result<(), String> {
    if let Some(library) = tune_plugin_native::provider(id) {
        run(
            &tune_plugin_native::NativeBatch(library),
            input,
            output,
            options,
            cancellation,
        )
    } else {
        run(fallback, input, output, options, cancellation)
    }
}

pub(super) fn run(
    tool: &dyn BatchTool,
    input: &Path,
    output: &Path,
    options: &serde_json::Value,
    cancellation: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut host = FileHost::new(input, output, cancellation);
    let result = tool
        .run(&mut host, &[SourceSelection::Track(0)], options)
        .map_err(|e| host.error.take().unwrap_or_else(|| e.to_string()))?;
    if result.state == JobState::Completed && result.artifacts.len() == 1 {
        Ok(())
    } else {
        Err(host
            .error
            .unwrap_or_else(|| format!("plugin job {:?}: {:?}", result.state, result.failures)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wav(path: &Path, samples: &[i16]) {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48000u32.to_le_bytes());
        bytes.extend_from_slice(&192000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn premium_sdk_real_flac_roundtrip_and_no_clobber() {
        tokio::task::spawn_blocking(|| {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("source.wav");
            let output = dir.path().join("output.flac");
            let samples: Vec<i16> = (0..2048)
                .map(|i| ((i as f32 * 0.04).sin() * 16000.0) as i16)
                .collect();
            wav(&input, &samples);
            let original = std::fs::read(&input).unwrap();
            let token = Arc::new(AtomicBool::new(false));
            run(
                &tune_plugin_converter::Converter,
                &input,
                &output,
                &serde_json::json!({"format":"flac"}),
                token.clone(),
            )
            .unwrap();
            let decoded = tune_core::audio::decode::decode_to_pcm(
                output.to_str().unwrap(),
                None,
                None,
                0.0,
                f64::MAX,
            )
            .unwrap();
            assert_eq!(
                decoded.samples_i32,
                samples.iter().map(|s| i32::from(*s)).collect::<Vec<_>>(),
                "conversion changed source PCM"
            );
            let before = std::fs::read(&output).unwrap();
            assert!(
                run(
                    &tune_plugin_converter::Converter,
                    &input,
                    &output,
                    &serde_json::json!({"format":"flac"}),
                    token
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read(&output).unwrap(),
                before,
                "destination was overwritten"
            );
            assert_eq!(
                std::fs::read(&input).unwrap(),
                original,
                "source was modified"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                2,
                "staging was leaked"
            );
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn premium_sdk_real_declick_and_cancelled_output() {
        tokio::task::spawn_blocking(|| {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("source.wav");
            let output = dir.path().join("clean.wav");
            wav(&input, &[0, 0, 0, 0, 0, 20000, 0, -20000, 0, 0]);
            run(
                &tune_plugin_declick::Declick,
                &input,
                &output,
                &serde_json::json!({"output_format":"wav","zero_cross":false}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let decoded = tune_core::audio::decode::decode_to_pcm(
                output.to_str().unwrap(),
                None,
                None,
                0.0,
                f64::MAX,
            )
            .unwrap();
            assert_eq!(
                decoded.samples_i32,
                vec![0, 20000, 0, -20000],
                "right-channel audio was trimmed"
            );
            let cancelled = dir.path().join("cancelled.wav");
            assert!(
                run(
                    &tune_plugin_converter::Converter,
                    &input,
                    &cancelled,
                    &serde_json::json!({"format":"wav"}),
                    Arc::new(AtomicBool::new(true))
                )
                .is_err()
            );
            assert!(!cancelled.exists());
        })
        .await
        .unwrap();
    }
    #[test]
    fn premium_sdk_rejects_forged_source_and_output_handles() {
        let mut host = FileHost::new(
            Path::new("must-not-open"),
            Path::new("must-not-write"),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(
            host.open(&SourceHandle("../other-job".into())),
            Err(Error::InvalidState)
        ));
        assert_eq!(
            host.finish(&WriterHandle("other-job".into())),
            Err(Error::InvalidState)
        );
    }
}
