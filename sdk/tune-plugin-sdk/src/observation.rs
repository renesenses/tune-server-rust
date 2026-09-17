//! Observations are read-only and independent of premium DSP installation.
//! A slow subscriber drops frames; it never backpressures the audio producer.
use crate::{Error, audio::AudioFormat};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPoint {
    DecodedSource,
    PreDsp,
    PostDsp,
    PreOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// PCM observed in the actual rendering pipeline at `point`.
    Pipeline,
    /// Independent decode for analysis; does NOT measure the renderer output.
    SourceProbe,
}

#[derive(Debug, Clone)]
pub struct ObservationStamp {
    pub zone_id: i64,
    pub generation: u64,
    pub position_frames: u64,
    /// Monotonic host time, not wall-clock Unix time.
    pub monotonic_ns: u64,
    pub format: AudioFormat,
    pub point: ObservationPoint,
    pub provenance: Provenance,
    pub dropped_frames: u64,
}

#[derive(Debug, Clone)]
pub struct SpectrumFrame {
    pub stamp: ObservationStamp,
    pub relative: Vec<f32>,
    pub dbfs: Vec<f32>,
    pub frequencies_hz: Vec<f32>,
    pub resolved: Vec<bool>,
    pub fft_size: usize,
    pub frames_analyzed: usize,
    pub resolution_hz: f32,
}

impl SpectrumFrame {
    /// Validate before publication. Never invent a post-DSP observation from
    /// a source probe, nor infer frequencies from a track's metadata.
    pub fn validate(&self) -> Result<(), Error> {
        let n = self.dbfs.len();
        if n == 0
            || self.relative.len() != n
            || self.frequencies_hz.len() != n
            || self.resolved.len() != n
            || self.frames_analyzed == 0
            || self.frames_analyzed > self.fft_size
            || !self.fft_size.is_power_of_two()
            || !self.resolution_hz.is_finite()
            || self.resolution_hz <= 0.0
        {
            return Err(Error::InvalidObservation);
        }
        if self.stamp.provenance == Provenance::SourceProbe
            && self.stamp.point != ObservationPoint::DecodedSource
        {
            return Err(Error::InvalidObservation);
        }
        let expected = self.stamp.format.sample_rate() as f32 / self.frames_analyzed as f32;
        if (self.resolution_hz - expected).abs() > expected * 1e-5 {
            return Err(Error::InvalidObservation);
        }
        if self.dbfs.iter().any(|v| !v.is_finite())
            || self
                .relative
                .iter()
                .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            || self.frequencies_hz.iter().any(|v| {
                !v.is_finite() || *v < 0.0 || *v > self.stamp.format.sample_rate() as f32 / 2.0
            })
            || self.frequencies_hz.windows(2).any(|v| v[0] > v[1])
        {
            return Err(Error::InvalidObservation);
        }
        Ok(())
    }
}

/// A subscription identity changes on track/seek/zone switch. Repeated
/// positions are allowed; a renderer can publish multiple points at one time.
pub struct ObservationCursor {
    zone_id: i64,
    generation: u64,
    position_frames: u64,
}

impl ObservationCursor {
    pub fn new(zone_id: i64, generation: u64) -> Self {
        Self {
            zone_id,
            generation,
            position_frames: 0,
        }
    }
    pub fn accepts(&mut self, stamp: &ObservationStamp) -> bool {
        if stamp.zone_id != self.zone_id
            || stamp.generation != self.generation
            || stamp.position_frames < self.position_frames
        {
            return false;
        }
        self.position_frames = stamp.position_frames;
        true
    }
}
