//! Compatibility adapter for Tune's existing byte/f32 producer stages. All
//! native instances and scratch space are prepared before processing. Device
//! callbacks remain outside this adapter. State persists across block splits.
use crate::{Library, NativeProcessor};
use std::sync::Arc;
use tune_plugin_sdk::{Error, Settings, audio::*};
const FRAMES: usize = 4096;
pub struct Stage {
    processors: Vec<NativeProcessor>,
    channels: u16,
    sample_rate: u32,
    floats: Vec<f32>,
    shorts: Vec<i16>,
    integers: Vec<i32>,
    bytes: Vec<u8>,
    last: usize,
    pub info: Settings,
    pub report: ProcessReport,
    pub samples_seen: u64,
}
impl Stage {
    pub fn prepare(
        library: Arc<Library>,
        sample_rate: u32,
        channels: u16,
        settings: &Settings,
    ) -> Result<Self, Error> {
        let layout = match channels {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            n => ChannelLayout::Discrete(n),
        };
        let mut processors = Vec::new();
        for encoding in [
            SampleEncoding::F32,
            SampleEncoding::S16,
            SampleEncoding::S24Le,
            SampleEncoding::S32,
        ] {
            processors.push(library.prepare(
                AudioFormat::new(sample_rate, layout, encoding)?,
                FRAMES,
                settings,
            )?);
        }
        let count = FRAMES * usize::from(channels);
        let info = processors[0].diagnostics();
        Ok(Self {
            info,
            processors,
            channels,
            sample_rate,
            floats: vec![0.0; count],
            shorts: vec![0; count],
            integers: vec![0; count],
            bytes: vec![0; count * 3],
            last: 0,
            report: ProcessReport::default(),
            samples_seen: 0,
        })
    }
    pub fn inherit(&mut self, previous: &Self) -> Result<(), Error> {
        if self.channels != previous.channels || self.sample_rate != previous.sample_rate {
            return Err(Error::InvalidFormat);
        }
        for (new, old) in self.processors.iter_mut().zip(&previous.processors) {
            new.inherit_from(old)?;
        }
        self.report = previous.report;
        self.samples_seen = previous.samples_seen;
        self.last = previous.last;
        Ok(())
    }
    pub fn diagnostics(&self) -> Settings {
        self.processors[self.last].diagnostics()
    }
    fn accumulate(&mut self, report: ProcessReport, samples: usize) {
        self.report.clipping = report.clipping;
        self.report.changed |= report.changed;
        self.report.clipped_samples = self
            .report
            .clipped_samples
            .saturating_add(report.clipped_samples);
        self.report.non_finite_samples = self
            .report
            .non_finite_samples
            .saturating_add(report.non_finite_samples);
        self.samples_seen = self.samples_seen.saturating_add(samples as u64);
    }
    pub fn process_f32(&mut self, samples: &mut [f32]) -> Result<ProcessReport, Error> {
        if !samples.len().is_multiple_of(usize::from(self.channels)) {
            return Err(Error::IncompleteFrame);
        }
        let mut total = ProcessReport::default();
        for chunk in samples.chunks_mut(self.floats.len()) {
            let scratch = &mut self.floats[..chunk.len()];
            scratch.copy_from_slice(chunk);
            let p = &mut self.processors[0];
            let f = p.format;
            let report = p.process(
                &mut AudioBlock::new(f, SamplesMut::F32(scratch), FRAMES)?,
                BlockContext {
                    zone_id: 0,
                    generation: 0,
                    position_frames: self.samples_seen / u64::from(self.channels),
                },
            )?;
            chunk.copy_from_slice(scratch);
            self.accumulate(report, chunk.len());
            total.changed |= report.changed;
            total.clipped_samples += report.clipped_samples;
            total.non_finite_samples += report.non_finite_samples;
        }
        self.last = 0;
        total.clipping = self.report.clipping;
        Ok(total)
    }
    pub fn process_pcm(&mut self, pcm: &mut [u8], depth: u16) -> Result<ProcessReport, Error> {
        let (index, width) = match depth {
            16 => (1, 2),
            24 => (2, 3),
            32 => (3, 4),
            _ => return Err(Error::UnsupportedFormat),
        };
        let frame_bytes = width * usize::from(self.channels);
        let valid = pcm.len() / frame_bytes * frame_bytes;
        let mut total = ProcessReport::default();
        for chunk in pcm[..valid].chunks_mut(FRAMES * frame_bytes) {
            let count = chunk.len() / width;
            let p = &mut self.processors[index];
            let context = BlockContext {
                zone_id: 0,
                generation: 0,
                position_frames: self.samples_seen / u64::from(self.channels),
            };
            let report = match depth {
                16 => {
                    let scratch = &mut self.shorts[..count];
                    for (s, b) in scratch.iter_mut().zip(chunk.as_chunks::<2>().0) {
                        *s = i16::from_le_bytes(*b);
                    }
                    let report = p.process(
                        &mut AudioBlock::new(p.format, SamplesMut::S16(scratch), FRAMES)?,
                        context,
                    )?;
                    for (s, b) in scratch.iter().zip(chunk.as_chunks_mut::<2>().0) {
                        *b = s.to_le_bytes();
                    }
                    report
                }
                24 => {
                    let scratch = &mut self.bytes[..chunk.len()];
                    scratch.copy_from_slice(chunk);
                    let report = p.process(
                        &mut AudioBlock::new(p.format, SamplesMut::S24Le(scratch), FRAMES)?,
                        context,
                    )?;
                    chunk.copy_from_slice(scratch);
                    report
                }
                32 => {
                    let scratch = &mut self.integers[..count];
                    for (s, b) in scratch.iter_mut().zip(chunk.as_chunks::<4>().0) {
                        *s = i32::from_le_bytes(*b);
                    }
                    let report = p.process(
                        &mut AudioBlock::new(p.format, SamplesMut::S32(scratch), FRAMES)?,
                        context,
                    )?;
                    for (s, b) in scratch.iter().zip(chunk.as_chunks_mut::<4>().0) {
                        *b = s.to_le_bytes();
                    }
                    report
                }
                _ => unreachable!(),
            };
            self.accumulate(report, count);
            total.changed |= report.changed;
            total.clipped_samples += report.clipped_samples;
            total.non_finite_samples += report.non_finite_samples;
        }
        self.last = index;
        total.clipping = self.report.clipping;
        Ok(total)
    }
}
