//! An in-memory host for contract tests. `pcm-test` is deliberately NOT named
//! FLAC/WAV: this host captures PCM and cannot certify a container encoder.
use std::collections::BTreeMap;
use tune_plugin_sdk::{
    Error,
    audio::{AudioFormat, SampleEncoding},
    batch::*,
};

#[derive(Clone)]
pub struct Source {
    pub format: AudioFormat,
    pub samples: Vec<i32>,
    pub metadata: BTreeMap<String, String>,
}

struct Reader {
    source: Source,
    offset: usize,
}
impl PcmReader for Reader {
    fn format(&self) -> AudioFormat {
        self.source.format
    }
    fn read_frames(&mut self, output: &mut [i32]) -> Result<usize, Error> {
        let channels = usize::from(self.source.format.channels());
        if output.len() < channels {
            return Err(Error::IncompleteFrame);
        }
        let count =
            (output.len() / channels * channels).min(self.source.samples.len() - self.offset);
        output[..count].copy_from_slice(&self.source.samples[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count / channels)
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), Error> {
        let frame = usize::try_from(frame).map_err(|_| Error::InvalidSettings)?;
        let offset = frame
            .checked_mul(usize::from(self.source.format.channels()))
            .ok_or(Error::InvalidSettings)?;
        if offset > self.source.samples.len() {
            return Err(Error::InvalidSettings);
        }
        self.offset = offset;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryHost {
    sources: BTreeMap<String, Source>,
    pending: BTreeMap<String, Source>,
    pub artifacts: BTreeMap<String, Source>,
    pub progress: Vec<(usize, usize, SourceHandle)>,
    /// Injection: cancel after N writes, including inside the first file.
    pub cancel_after_writes: Option<usize>,
    writes: usize,
    next_output: u64,
}

impl MemoryHost {
    pub fn insert_track(&mut self, id: i64, source: Source) -> Result<(), Error> {
        if !matches!(
            source.format.encoding(),
            SampleEncoding::S16 | SampleEncoding::S24Le | SampleEncoding::S32
        ) {
            return Err(Error::UnsupportedFormat);
        }
        if !source
            .samples
            .len()
            .is_multiple_of(usize::from(source.format.channels()))
        {
            return Err(Error::IncompleteFrame);
        }
        self.sources.insert(format!("track:{id}"), source);
        Ok(())
    }
    pub fn source(&self, id: i64) -> &Source {
        &self.sources[&format!("track:{id}")]
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl BatchHost for MemoryHost {
    fn resolve(&mut self, selections: &[SourceSelection]) -> Result<Vec<SourceHandle>, Error> {
        let mut sources = Vec::new();
        for selection in selections {
            let SourceSelection::Track(id) = selection else {
                return Err(Error::CapabilityMissing);
            };
            let key = format!("track:{id}");
            if !self.sources.contains_key(&key) {
                return Err(Error::HostFailure);
            }
            sources.push(SourceHandle(key));
        }
        Ok(sources)
    }
    fn codecs(&self) -> Vec<CodecCapability> {
        vec![CodecCapability {
            codec: "pcm-test".into(),
            sample_rates: vec![44_100, 48_000, 96_000, 192_000],
            bit_depths: vec![16, 24, 32],
            qualities: vec![],
        }]
    }
    fn cancelled(&self) -> bool {
        self.cancel_after_writes.is_some_and(|n| self.writes >= n)
    }
    fn open(&mut self, source: &SourceHandle) -> Result<Box<dyn PcmReader>, Error> {
        let source = self
            .sources
            .get(&source.0)
            .ok_or(Error::HostFailure)?
            .clone();
        Ok(Box::new(Reader { source, offset: 0 }))
    }
    fn create_output(
        &mut self,
        source: &SourceHandle,
        options: &EncodeOptions,
        destination: &Destination,
    ) -> Result<WriterHandle, Error> {
        if !self.codecs().iter().any(|c| c.supports(options))
            || *destination != Destination::Download
        {
            return Err(Error::CapabilityMissing);
        }
        let source = self.sources.get(&source.0).ok_or(Error::HostFailure)?;
        let expected_depth = match source.format.encoding() {
            SampleEncoding::S16 => 16,
            SampleEncoding::S24Le => 24,
            SampleEncoding::S32 => 32,
            _ => return Err(Error::UnsupportedFormat),
        };
        if options.sample_rate != source.format.sample_rate() || options.bit_depth != expected_depth
        {
            return Err(Error::UnsupportedFormat);
        }
        let key = format!("output:{}", self.next_output);
        self.next_output += 1;
        self.pending.insert(
            key.clone(),
            Source {
                format: source.format,
                samples: vec![],
                metadata: BTreeMap::new(),
            },
        );
        Ok(WriterHandle(key))
    }
    fn write_frames(&mut self, output: &WriterHandle, samples: &[i32]) -> Result<(), Error> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        let target = self.pending.get_mut(&output.0).ok_or(Error::HostFailure)?;
        if !samples
            .len()
            .is_multiple_of(usize::from(target.format.channels()))
        {
            return Err(Error::IncompleteFrame);
        }
        target.samples.extend_from_slice(samples);
        self.writes += 1;
        Ok(())
    }
    fn copy_metadata(&mut self, source: &SourceHandle, output: &WriterHandle) -> Result<(), Error> {
        let metadata = self
            .sources
            .get(&source.0)
            .ok_or(Error::HostFailure)?
            .metadata
            .clone();
        self.pending
            .get_mut(&output.0)
            .ok_or(Error::HostFailure)?
            .metadata = metadata;
        Ok(())
    }
    fn finish(&mut self, output: &WriterHandle) -> Result<ArtifactHandle, Error> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        let source = self.pending.remove(&output.0).ok_or(Error::HostFailure)?;
        self.artifacts.insert(output.0.clone(), source);
        Ok(ArtifactHandle(output.0.clone()))
    }
    fn abort(&mut self, output: &WriterHandle) {
        self.pending.remove(&output.0);
    }
    fn progress(&mut self, completed: usize, total: usize, current: &SourceHandle) {
        self.progress.push((completed, total, current.clone()));
    }
}
