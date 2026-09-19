//! Deterministic test hosts, not adapters to Tune's production pipeline.
//! Capture proves sample processing through the SDK, not a DAC or codec.
#![forbid(unsafe_code)]
pub mod batch;

use std::collections::VecDeque;
use tune_plugin_sdk::{Error, Settings, audio::*, observation::SpectrumFrame};

pub struct Capture {
    pub samples: Vec<f32>,
    pub reports: Vec<ProcessReport>,
}

/// Exercise planning, preparation, multiple blocks and end-of-stream. Bypass
/// returns the original bits without instantiating a plugin or converting PCM.
pub fn render_f32(
    factory: &dyn DspFactory,
    context: PlaybackContext,
    licensed: bool,
    settings: &Settings,
    format: AudioFormat,
    input: &[f32],
    chunk_frames: usize,
) -> Result<Capture, Error> {
    if format.encoding() != SampleEncoding::F32 {
        return Err(Error::UnsupportedFormat);
    }
    if chunk_frames == 0 {
        return Err(Error::BlockTooLarge);
    }
    let channels = usize::from(format.channels());
    if !input.len().is_multiple_of(channels) {
        return Err(Error::IncompleteFrame);
    }
    let chunk_samples = chunk_frames
        .checked_mul(channels)
        .ok_or(Error::BlockTooLarge)?;
    let mut capture = Capture {
        samples: Vec::new(),
        reports: Vec::new(),
    };
    if policy_bypass(&context, licensed).is_some() {
        capture.samples.extend_from_slice(input);
        return Ok(capture);
    }
    match factory.assess(&context, settings)? {
        Applicability::Bypass(_) => {
            capture.samples.extend_from_slice(input);
            return Ok(capture);
        }
        Applicability::Unsupported { .. } => return Err(Error::UnsupportedFormat),
        Applicability::Process { .. } => {}
    }
    let mut processor = factory.prepare(format, chunk_frames, settings)?;
    for (index, chunk) in input.chunks(chunk_samples).enumerate() {
        let mut samples = chunk.to_vec();
        let mut block = AudioBlock::new(format, SamplesMut::F32(&mut samples), chunk_frames)?;
        let report = processor.process(
            &mut block,
            BlockContext {
                zone_id: context.zone_id,
                generation: 1,
                position_frames: (index * chunk_frames) as u64,
            },
        )?;
        if samples.iter().any(|s| !s.is_finite()) {
            return Err(Error::NonFinite);
        }
        capture.samples.extend(samples);
        capture.reports.push(report);
    }
    // A broken drain must fail rather than hang a CI worker indefinitely.
    for _ in 0..1024 {
        let mut samples = vec![0.0; chunk_samples];
        let mut block = AudioBlock::new(format, SamplesMut::F32(&mut samples), chunk_frames)?;
        let report = processor.drain(&mut block)?;
        if report.frames_written > chunk_frames || (!report.complete && report.frames_written == 0)
        {
            return Err(Error::InvalidState);
        }
        let written = &samples[..report.frames_written * channels];
        if written.iter().any(|s| !s.is_finite()) {
            return Err(Error::NonFinite);
        }
        capture.samples.extend_from_slice(written);
        if report.complete {
            return Ok(capture);
        }
    }
    Err(Error::InvalidState)
}

/// Compare in the sample domain, with both absolute and relative tolerances.
/// Tolerances must themselves be finite and non-negative.
pub fn assert_pcm_close(actual: &[f32], expected: &[f32], absolute: f32, relative: f32) {
    assert!(absolute.is_finite() && relative.is_finite() && absolute >= 0.0 && relative >= 0.0);
    assert_eq!(actual.len(), expected.len(), "PCM frame count changed");
    for (index, (&a, &e)) in actual.iter().zip(expected).enumerate() {
        assert!(
            a.is_finite() && e.is_finite() && (a - e).abs() <= absolute + relative * e.abs(),
            "PCM mismatch at sample {index}: actual={a}, expected={e}"
        );
    }
}

/// Model the observation queue's drop-oldest policy, off the audio thread.
/// It intentionally has no plugin registry or entitlement dependency.
pub struct ObservationQueue {
    queue: VecDeque<SpectrumFrame>,
    capacity: usize,
    dropped: u64,
}

impl ObservationQueue {
    pub fn new(capacity: usize) -> Result<Self, Error> {
        if capacity == 0 {
            return Err(Error::InvalidSettings);
        }
        Ok(Self {
            queue: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        })
    }
    pub fn publish(&mut self, mut frame: SpectrumFrame) -> Result<(), Error> {
        frame.validate()?;
        if self.queue.len() == self.capacity {
            self.queue.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        frame.stamp.dropped_frames = frame.stamp.dropped_frames.saturating_add(self.dropped);
        self.queue.push_back(frame);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<SpectrumFrame> {
        self.queue.pop_front()
    }
}
