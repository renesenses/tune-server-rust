//! Per-zone PCM audio tap.
//!
//! A minimal, generic core primitive that exposes the **decoded PCM signal** to
//! more than one consumer. Plugins (WASM) can receive bus events, query the DB,
//! expose routes and register outputs — but they cannot see decoded PCM, and
//! spectrum / loudness / waveform all need the raw signal. So the tap lives in
//! core: a per-zone [`tokio::sync::broadcast`] of PCM windows that the core
//! levels/spectrum forwarder and any signal-analysis plugin subscribe to.
//!
//! Design notes:
//! - **Separate from the JSON event bus.** The event bus carries discrete
//!   control events; PCM never transits it as JSON/base64. The tap is a
//!   dedicated binary broadcast.
//! - **`broadcast` (not `mpsc`)** so N consumers fan out; the bounded ring makes
//!   a slow consumer *lag/drop* old frames instead of back-pressuring the audio
//!   path.
//! - **Opt-in / zero-cost when idle.** With no subscriber, publishing is skipped
//!   (`receiver_count() == 0`), so behaviour is identical to today when no plugin
//!   wants PCM.
//!
//! # Wiring (follow-up, kept out of this scaffold)
//! - `Playback` holds one [`ZoneTap`] per active zone (same place as
//!   `current_play_seq(zone_id)`), and hands a [`PcmPublisher`] to the decoder.
//! - `decode_to_pcm_streaming_inner` publishes at the **four** sites where
//!   #1105 calls `levels::send_windowed_levels(...)` — same `pcm: &[u8]` +
//!   `bit_depth`/`channels`/`sample_rate` already in hand.
//! - The `playback.audio_levels` forwarder (#1105) becomes a **consumer** of the
//!   tap: it `subscribe()`s, runs `compute_levels` off the decode hot path, and
//!   keeps its existing pacing / play_seq supersession logic.
//! - The plugin SDK exposes the tap to opted-in plugins via a host callback
//!   `on_pcm(zone_id, ptr, len, format, position_ms, play_seq)`.
#![allow(dead_code)] // scaffold: consumed once wired into decode.rs + Playback

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

/// Ring capacity, in windows. ~64 * 40 ms ≈ 2.5 s of slack before a lagging
/// consumer starts dropping.
pub const TAP_CAPACITY: usize = 64;

/// Window granularity of the tap, in milliseconds. Matches #1105's
/// `send_windowed_levels` (25–32 Hz, exploitable by a real-time visualizer).
/// A consumer needing a specific FFT size (e.g. 1024) rebuffers on its side.
pub const WINDOW_MS: u64 = 40;

/// How the raw interleaved bytes in [`PcmTapFrame::pcm`] are laid out. Do not
/// infer this from `bit_depth` alone (s24 vs f32 both exist).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    /// Signed integer, little-endian, width = `bit_depth` (16/24/32).
    SignedInt,
    /// 32-bit float, little-endian.
    Float,
}

/// Everything a consumer needs to interpret [`PcmTapFrame::pcm`].
#[derive(Clone, Copy, Debug)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
    pub sample_format: SampleFormat,
}

impl PcmFormat {
    /// Bytes per interleaved frame (all channels). 0 if the format is unusable.
    pub fn frame_bytes(&self) -> usize {
        (self.bit_depth as usize / 8) * self.channels as usize
    }
}

/// One PCM window published on the tap.
#[derive(Clone, Debug)]
pub struct PcmTapFrame {
    pub zone_id: i64,
    /// Raw interleaved PCM for this window. `Arc` → O(1) clone per broadcast
    /// subscriber (no per-consumer copy of the samples).
    pub pcm: Arc<[u8]>,
    pub format: PcmFormat,
    /// Position of the **start** of this window within the track. Lets a
    /// visualizer align to the renderer's reported position instead of the
    /// decode clock (buffer-depth correction — the "known limit" flagged on
    /// #1105). Carried from v1 to avoid a later breaking change.
    pub track_position: Duration,
    /// Duration of this window (≈ [`WINDOW_MS`], shorter for the tail chunk).
    pub window: Duration,
    /// Supersession counter for the zone. A consumer drops frames whose
    /// `play_seq` no longer matches the current track (avoids two successive
    /// tracks emitting in parallel).
    pub play_seq: u64,
}

/// The per-zone broadcast endpoint. Held by `Playback`; fans out to the core
/// levels forwarder and any PCM-hungry plugin.
pub struct ZoneTap {
    sender: broadcast::Sender<PcmTapFrame>,
}

impl Default for ZoneTap {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoneTap {
    pub fn new() -> Self {
        Self {
            sender: broadcast::channel(TAP_CAPACITY).0,
        }
    }

    /// Subscribe a consumer (core forwarder or plugin bridge). `recv()` yields
    /// `Err(RecvError::Lagged)` when the consumer fell behind — skip and carry
    /// on; that is the intended real-time behaviour.
    pub fn subscribe(&self) -> broadcast::Receiver<PcmTapFrame> {
        self.sender.subscribe()
    }

    /// Number of live consumers — used to skip publishing entirely when idle.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// A publisher for one track playthrough, handed to the decoder.
    pub fn publisher(&self, zone_id: i64, format: PcmFormat, play_seq: u64) -> PcmPublisher {
        PcmPublisher {
            sender: self.sender.clone(),
            zone_id,
            format,
            play_seq,
            position: Duration::ZERO,
        }
    }
}

/// Handed to the decoder for the duration of a track. Splits raw decoder PCM
/// into ~[`WINDOW_MS`] windows and broadcasts them, advancing the track
/// position. This is the raw-PCM twin of #1105's `send_windowed_levels`, minus
/// the `compute_levels` — the analysis moves to the consumer side.
pub struct PcmPublisher {
    sender: broadcast::Sender<PcmTapFrame>,
    zone_id: i64,
    format: PcmFormat,
    play_seq: u64,
    position: Duration,
}

impl PcmPublisher {
    /// Window `pcm` and broadcast each window. No-op (but position still
    /// advances) when nobody is subscribed, so the audio path pays nothing.
    pub fn send_windowed(&mut self, pcm: &[u8]) {
        let frame_bytes = self.format.frame_bytes();
        if frame_bytes == 0 || self.format.sample_rate == 0 {
            return;
        }
        let idle = self.sender.receiver_count() == 0;
        let frames_per_window =
            (self.format.sample_rate as usize * WINDOW_MS as usize / 1000).max(1);
        let window_bytes = frames_per_window * frame_bytes;

        for chunk in pcm.chunks(window_bytes) {
            let frames = chunk.len() / frame_bytes;
            let window = Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64);
            if !idle {
                // TODO(perf): thread an `Arc<[u8]>` down from the decoder to drop
                // this per-window copy. A 40 ms copy is cheap, so it is fine for
                // the scaffold.
                let _ = self.sender.send(PcmTapFrame {
                    zone_id: self.zone_id,
                    pcm: Arc::from(chunk.to_vec().into_boxed_slice()),
                    format: self.format,
                    track_position: self.position,
                    window,
                    play_seq: self.play_seq,
                });
            }
            self.position += window;
        }
    }

    /// Start-of-window position most recently reached (for logging/tests).
    pub fn position(&self) -> Duration {
        self.position
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sr: u32) -> PcmFormat {
        PcmFormat {
            sample_rate: sr,
            channels: 2,
            bit_depth: 16,
            sample_format: SampleFormat::SignedInt,
        }
    }

    #[tokio::test]
    async fn windows_split_at_40ms_and_carry_position() {
        let tap = ZoneTap::new();
        let mut rx = tap.subscribe();
        let sr = 48_000;
        let mut pub_ = tap.publisher(7, fmt(sr), 1);

        // 1 s of silence, 16-bit stereo = sr * 4 bytes.
        pub_.send_windowed(&vec![0u8; sr as usize * 4]);

        // 1000 ms / 40 ms = 25 windows.
        let mut n = 0;
        let mut total = Duration::ZERO;
        let mut last_pos = Duration::ZERO;
        while let Ok(f) = rx.try_recv() {
            assert_eq!(f.zone_id, 7);
            assert_eq!(f.play_seq, 1);
            assert!(f.track_position >= last_pos, "position is monotonic");
            last_pos = f.track_position;
            total += f.window;
            n += 1;
        }
        assert_eq!(n, 25);
        assert!(
            (total.as_secs_f64() - 1.0).abs() < 1e-6,
            "total duration preserved"
        );
    }

    #[tokio::test]
    async fn idle_tap_still_advances_position_but_sends_nothing() {
        let tap = ZoneTap::new();
        let sr = 44_100;
        let mut pub_ = tap.publisher(1, fmt(sr), 0);
        // No subscriber.
        pub_.send_windowed(&vec![0u8; sr as usize * 4]);
        assert!((pub_.position().as_secs_f64() - 1.0).abs() < 1e-6);
        assert_eq!(tap.receiver_count(), 0);
    }
}
