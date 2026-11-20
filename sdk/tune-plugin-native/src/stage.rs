//! Compatibility adapter for Tune's existing byte/f32 producer stages, and the
//! `f64` entry of TESS (T05). All native instances and scratch space are
//! prepared before processing. Device callbacks remain outside this adapter.
//! State persists across block splits.
//!
//! F64 negotiation (ABI 1 unchanged): a plugin whose manifest requests
//! [`tune_plugin_sdk::manifest::AUDIO_F64`] gets a fifth, `F64` instance and
//! [`Stage::process_f64`] hands it the host's `f64` samples as they are. Any
//! other plugin keeps exactly its four historical instances; `process_f64`
//! then converts at the plugin boundary (`f64` -> `f32`, the `F32` instance,
//! `f32` -> `f64`), so an existing `F32` package runs unchanged.
use crate::{Library, NativeProcessor};
use std::sync::Arc;
use tune_plugin_sdk::{Error, Settings, audio::*};
const FRAMES: usize = 4096;
/// Index of the native `F64` instance, after F32, S16, S24Le and S32.
const F64_INDEX: usize = 4;
pub struct Stage {
    processors: Vec<NativeProcessor>,
    channels: u16,
    sample_rate: u32,
    floats: Vec<f32>,
    /// Scratch of the native `F64` instance; empty without one.
    doubles: Vec<f64>,
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
        // T05: only a plugin that asked for F64 is ever given F64. One that
        // asked and refuses it is refused here too, with its real settings.
        let native_f64 = library.manifest.declares_f64();
        if native_f64 {
            processors.push(
                library
                    .prepare(
                        AudioFormat::new(sample_rate, layout, SampleEncoding::F64)?,
                        FRAMES,
                        settings,
                    )
                    .map_err(|e| match e {
                        Error::UnsupportedFormat | Error::InvalidFormat => Error::CapabilityMissing,
                        other => other,
                    })?,
            );
        }
        let count = FRAMES * usize::from(channels);
        let info = processors[0].diagnostics();
        Ok(Self {
            info,
            processors,
            channels,
            sample_rate,
            floats: vec![0.0; count],
            doubles: if native_f64 {
                vec![0.0; count]
            } else {
                Vec::new()
            },
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
    /// TESS T05 — the plugin itself processes `F64` (it requested
    /// `audio-f64`); `false` means [`Self::process_f64`] converts through
    /// `f32` at its boundary.
    pub fn processes_f64_natively(&self) -> bool {
        self.processors.len() > F64_INDEX
    }
    /// TESS T05 — process interleaved `f64` samples, full scale ±1.0.
    /// Natively for a plugin that requested `audio-f64`; otherwise through
    /// its `F32` instance, converted at the boundary. On error the samples of
    /// the failing block are left untouched, as with [`Self::process_f32`].
    pub fn process_f64(&mut self, samples: &mut [f64]) -> Result<ProcessReport, Error> {
        if !samples.len().is_multiple_of(usize::from(self.channels)) {
            return Err(Error::IncompleteFrame);
        }
        let native = self.processes_f64_natively();
        let index = if native { F64_INDEX } else { 0 };
        let mut total = ProcessReport::default();
        for chunk in samples.chunks_mut(self.floats.len()) {
            let context = BlockContext {
                zone_id: 0,
                generation: 0,
                position_frames: self.samples_seen / u64::from(self.channels),
            };
            let p = &mut self.processors[index];
            let f = p.format;
            let report = if native {
                let scratch = &mut self.doubles[..chunk.len()];
                scratch.copy_from_slice(chunk);
                let report = p.process(
                    &mut AudioBlock::new(f, SamplesMut::F64(scratch), FRAMES)?,
                    context,
                )?;
                chunk.copy_from_slice(scratch);
                report
            } else {
                let scratch = &mut self.floats[..chunk.len()];
                for (s, d) in scratch.iter_mut().zip(chunk.iter()) {
                    *s = *d as f32;
                }
                let report = p.process(
                    &mut AudioBlock::new(f, SamplesMut::F32(scratch), FRAMES)?,
                    context,
                )?;
                for (d, s) in chunk.iter_mut().zip(scratch.iter()) {
                    *d = f64::from(*s);
                }
                report
            };
            self.accumulate(report, chunk.len());
            total.changed |= report.changed;
            total.clipped_samples += report.clipped_samples;
            total.non_finite_samples += report.non_finite_samples;
        }
        self.last = index;
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
