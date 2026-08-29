//! Sample-accurate crossfade primitives for the local PCM output.
//!
//! This module deliberately knows nothing about [`OutputTarget`](crate::outputs::OutputTarget)
//! or device volume. A crossfade is part of the PCM signal path: changing a
//! renderer or DAC's persistent volume is never an acceptable substitute.

use std::collections::VecDeque;
use std::f32::consts::FRAC_PI_2;

/// Keeps the last `duration` worth of interleaved PCM out of the render ring.
///
/// Once the next producer is ready, that tail can be mixed with its head. If
/// no compatible next producer arrives, [`drain`](Self::drain) returns the
/// samples untouched, so enabling crossfade cannot truncate a final track.
#[derive(Debug)]
pub struct CrossfadeTail {
    capacity_samples: usize,
    samples: VecDeque<f32>,
}

impl CrossfadeTail {
    pub fn new(duration_s: f64, sample_rate: u32, channels: u16) -> Self {
        let frames = (duration_s.max(0.0) * sample_rate as f64).round() as usize;
        Self {
            capacity_samples: frames.saturating_mul(channels.max(1) as usize),
            samples: VecDeque::with_capacity(frames.saturating_mul(channels.max(1) as usize)),
        }
    }

    pub fn disabled() -> Self {
        Self {
            capacity_samples: 0,
            samples: VecDeque::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.capacity_samples > 0
    }

    /// Retain the newest tail and return older samples ready for the renderer.
    pub fn push(&mut self, incoming: Vec<f32>) -> Vec<f32> {
        if self.capacity_samples == 0 {
            return incoming;
        }
        self.samples.extend(incoming);
        let ready_len = self.samples.len().saturating_sub(self.capacity_samples);
        self.samples.drain(..ready_len).collect()
    }

    /// Flush the retained tail and permanently bypass this transition when
    /// the payload cannot be mixed (notably DoP, which is not PCM audio).
    pub fn bypass_with(&mut self, incoming: Vec<f32>) -> Vec<f32> {
        let mut ready = self.drain();
        ready.extend(incoming);
        self.capacity_samples = 0;
        ready
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn drain(&mut self) -> Vec<f32> {
        self.samples.drain(..).collect()
    }
}

/// Result of an equal-power overlap between two interleaved PCM producers.
#[derive(Debug, PartialEq)]
pub struct CrossfadeMix {
    pub tail_prefix: Vec<f32>,
    pub overlap: Vec<f32>,
    pub head_suffix: Vec<f32>,
}

/// Mix the retained tail of the current track with the head of the next one.
///
/// The envelope is equal-power (`cos` / `sin`), evaluated once per frame and
/// applied to every channel. The overlap contains exactly `min(tail, head)`
/// complete frames; unmatched samples are returned without modification.
pub fn equal_power_crossfade(tail: &[f32], head: &[f32], channels: u16) -> CrossfadeMix {
    let channels = channels.max(1) as usize;
    let tail_frames = tail.len() / channels;
    let head_frames = head.len() / channels;
    let overlap_frames = tail_frames.min(head_frames);
    let tail_prefix_frames = tail_frames.saturating_sub(overlap_frames);
    let tail_prefix_samples = tail_prefix_frames * channels;
    let overlap_samples = overlap_frames * channels;

    let mut overlap = Vec::with_capacity(overlap_samples);
    for frame in 0..overlap_frames {
        // Include both exact boundaries when at least two frames overlap.
        let progress = if overlap_frames <= 1 {
            0.5
        } else {
            frame as f32 / (overlap_frames - 1) as f32
        };
        let outgoing_gain = (progress * FRAC_PI_2).cos();
        let incoming_gain = (progress * FRAC_PI_2).sin();
        let tail_start = tail_prefix_samples + frame * channels;
        let head_start = frame * channels;
        for channel in 0..channels {
            overlap.push(
                tail[tail_start + channel] * outgoing_gain
                    + head[head_start + channel] * incoming_gain,
            );
        }
    }

    CrossfadeMix {
        tail_prefix: tail[..tail_prefix_samples].to_vec(),
        overlap,
        head_suffix: head[overlap_samples..].to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_never_drops_the_end_when_no_next_track_arrives() {
        let mut tail = CrossfadeTail::new(1.0, 4, 1);
        assert_eq!(tail.push(vec![1.0, 2.0, 3.0]), Vec::<f32>::new());
        assert_eq!(tail.push(vec![4.0, 5.0]), vec![1.0]);
        assert_eq!(tail.drain(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn disabled_tail_is_an_identity_operation() {
        let mut tail = CrossfadeTail::disabled();
        assert_eq!(tail.push(vec![0.25, -0.5]), vec![0.25, -0.5]);
        assert!(tail.drain().is_empty());
    }

    /// #2211 — the capture contains both producers throughout the overlap,
    /// begins exactly on producer A, ends exactly on producer B, and contains
    /// the requested number of complete stereo frames.
    #[test]
    fn equal_power_capture_contains_both_tracks_and_exact_boundaries() {
        let tail = vec![1.0, 0.5, 1.0, 0.5, 1.0, 0.5, 1.0, 0.5];
        let head = vec![0.25, -1.0, 0.25, -1.0, 0.25, -1.0, 0.25, -1.0];
        let mixed = equal_power_crossfade(&tail, &head, 2);

        assert!(mixed.tail_prefix.is_empty());
        assert!(mixed.head_suffix.is_empty());
        assert_eq!(mixed.overlap.len(), 8);
        assert_eq!(&mixed.overlap[..2], &tail[..2]);
        assert!((mixed.overlap[6] - head[6]).abs() < 1.0e-6);
        assert!((mixed.overlap[7] - head[7]).abs() < 1.0e-6);
        // An interior frame cannot equal either isolated producer.
        assert_ne!(&mixed.overlap[2..4], &tail[2..4]);
        assert_ne!(&mixed.overlap[2..4], &head[2..4]);
    }

    #[test]
    fn a_short_next_track_preserves_the_unmatched_outgoing_prefix() {
        let mixed = equal_power_crossfade(&[1.0, 2.0, 3.0, 4.0], &[9.0, 8.0], 1);
        assert_eq!(mixed.tail_prefix, vec![1.0, 2.0]);
        assert_eq!(mixed.overlap.len(), 2);
        assert!(mixed.head_suffix.is_empty());
    }
}
