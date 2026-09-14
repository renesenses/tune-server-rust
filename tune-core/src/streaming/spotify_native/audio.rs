//! Finite, bounded PCM producer. Tune owns the queue and the HTTP session.
//! The sink is cancelled on stop/replacement/logout, including while backpressured.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    config::{Bitrate, PlayerConfig},
    convert::Converter,
    decoder::AudioPacket,
    mixer::NoOpVolume,
    player::{Player, PlayerEvent},
};
use tokio::sync::{Notify, mpsc, oneshot};

use super::{SpotifyNativeService, catalog};
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::{PlayRequest, ResolvedStream};

pub(super) struct Lease {
    zone: i64,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct PcmSink {
    tx: mpsc::Sender<Vec<u8>>,
    cancelled: Arc<AtomicBool>,
    ready: Arc<Notify>,
    started: Option<oneshot::Sender<Result<(), String>>>,
}

fn pcm_s16le(samples: &[f64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        // NaN is silence; Rust's saturating float cast prevents full-scale wrap.
        let sample = if sample.is_finite() { *sample } else { 0.0 };
        bytes.extend_from_slice(&((sample * 32768.0).round() as i16).to_le_bytes());
    }
    bytes
}

impl PcmSink {
    fn send(&self, mut bytes: Vec<u8>) -> SinkResult<()> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(SinkError::NotConnected("Tune stream cancelled".into()));
            }
            match self.tx.try_send(bytes) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(SinkError::NotConnected("Tune stream closed".into()));
                }
                Err(mpsc::error::TrySendError::Full(value)) => {
                    bytes = value;
                    // This is librespot's dedicated audio thread, not Tokio.
                    // Never block in blocking_send: cancellation must wake a full sink.
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

impl Sink for PcmSink {
    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        let AudioPacket::Samples(samples) = packet else {
            return Err(SinkError::InvalidParams(
                "Spotify raw packets are not PCM".into(),
            ));
        };
        if samples.len() % NUM_CHANNELS as usize != 0 {
            return Err(SinkError::InvalidParams(
                "Spotify PCM is not frame-aligned".into(),
            ));
        }
        // 4096-byte chunks, 64 slots: less than 1.5 seconds of stereo PCM.
        for chunk in samples.chunks(2048) {
            self.send(pcm_s16le(chunk))?;
            if let Some(started) = self.started.take() {
                self.ready.notify_one();
                let _ = started.send(Ok(()));
            }
        }
        Ok(())
    }
}

/// Cancels a producer if resolving play is itself dropped (e.g. superseded).
struct Pending {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

impl SpotifyNativeService {
    pub async fn resolve_audio(
        &self,
        streamer: Arc<AudioStreamer>,
        server_ip: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .as_deref()
            .ok_or("Spotify track id is required")?;
        let uri = catalog::uri(source_id, "track").map_err(|e| e.to_string())?;
        let session = self.session().await.map_err(|e| e.to_string())?;
        let track = catalog::track(&session, source_id)
            .await
            .map_err(|e| e.to_string())?;
        let seek_ms = u32::try_from(req.seek_ms.unwrap_or(0))
            .map_err(|_| "Spotify seek exceeds supported range")?;
        if track.duration_ms == 0 || seek_ms as u64 >= track.duration_ms {
            return Err("Spotify track duration or seek position is invalid".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.audio.lock().unwrap();
            if active.as_ref().is_some_and(|lease| {
                lease.zone != req.zone_id && !lease.cancelled.load(Ordering::Acquire)
            }) {
                return Err(
                    "Spotify native prototype permits one active zone; stop the other zone first"
                        .into(),
                );
            }
            *active = Some(Lease {
                zone: req.zone_id,
                cancelled: cancelled.clone(),
            });
        }
        let mut pending = Pending {
            cancelled: cancelled.clone(),
            armed: true,
        };
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: SAMPLE_RATE,
            bit_depth: 16,
            channels: NUM_CHANNELS as u16,
            duration_ms: Some(track.duration_ms - seek_ms as u64),
            ..Default::default()
        };
        let (stream_id, tx, ready) = streamer.create_session(info, false, 64).await;
        let (started, mut startup) = oneshot::channel();
        let sink = PcmSink {
            tx,
            cancelled: cancelled.clone(),
            ready,
            started: Some(started),
        };
        let player = Player::new(
            PlayerConfig {
                bitrate: Bitrate::Bitrate320,
                normalisation: false,
                gapless: false,
                ..Default::default()
            },
            session.clone(),
            Box::new(NoOpVolume),
            move || Box::new(sink),
        );
        let mut events = player.get_player_event_channel();
        player.load(uri, true, seek_ms);
        let stream_task = streamer.clone();
        let task_id = stream_id.clone();
        let cancel_task = cancelled.clone();
        let active = self.audio.clone();
        let (failed, mut failure) = oneshot::channel::<String>();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            let error = loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if cancel_task.load(Ordering::Acquire) || session.is_invalid() || !stream_task.session_alive(&task_id).await {
                            break Some("Spotify playback session ended or was cancelled".to_owned());
                        }
                    }
                    event = events.recv() => match event {
                        Some(PlayerEvent::EndOfTrack { .. }) => break None,
                        Some(PlayerEvent::Unavailable { .. }) => break Some("Spotify track is unavailable for this account".to_owned()),
                        None => break Some("Spotify player stopped unexpectedly".to_owned()),
                        _ => {}
                    }
                }
            };
            cancel_task.store(true, Ordering::Release);
            player.stop();
            drop(player);
            stream_task.end_session_input(&task_id).await;
            {
                let mut current = active.lock().unwrap();
                if current
                    .as_ref()
                    .is_some_and(|lease| Arc::ptr_eq(&lease.cancelled, &cancel_task))
                {
                    current.take();
                }
            }
            if let Some(error) = error {
                tracing::warn!(stream_id = %task_id, "spotify_native_producer_ended_early");
                let _ = failed.send(error);
                stream_task.remove_session(&task_id).await;
            }
        });
        let result = tokio::select! {
            result = &mut startup => result.unwrap_or_else(|_| Err("Spotify decoder produced no PCM".into())),
            result = &mut failure => Err(result.unwrap_or_else(|_| "Spotify track ended before playback began".into())),
            _ = tokio::time::sleep(Duration::from_secs(30)) => Err("Spotify decoder startup timed out".into()),
        };
        if let Err(error) = result {
            streamer.remove_session(&stream_id).await;
            return Err(error);
        }
        pending.armed = false;
        Ok(ResolvedStream {
            url: streamer.get_stream_url(&stream_id, server_ip, "wav"),
            mime_type: "audio/wav".into(),
            title: track.title,
            artist: Some(track.artist),
            album: track.album,
            duration_ms: Some(track.duration_ms as i64),
            source: "spotify".into(),
            cover_url: track.cover_path,
            stream_id: Some(stream_id),
            file_size: None,
            sample_rate: Some(SAMPLE_RATE),
            bit_depth: Some(16),
            channels: Some(NUM_CHANNELS as u32),
            origin_url: None,
            // 320 is requested, not measured. Do not claim negotiated bitrate.
            bitrate_kbps: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pcm_is_little_endian_saturating_and_nan_safe() {
        let bytes = pcm_s16le(&[-1.0, 0.5, 1.0, -2.0, f64::NAN]);
        assert_eq!(
            bytes,
            [0, 128, 0, 64, 255, 127, 0, 128, 0, 0],
            "PCM must not wrap full scale or swap byte order"
        );
    }
    #[tokio::test]
    async fn native_cancel_unblocks_full_sink() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send(vec![0; 4]).await.unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let task = tokio::task::spawn_blocking(move || {
            PcmSink {
                tx,
                cancelled,
                ready: Arc::new(Notify::new()),
                started: None,
            }
            .send(vec![0; 4])
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        signal.store(true, Ordering::Release);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("cancellation must unblock a full Spotify sink")
                .unwrap()
                .is_err()
        );
    }
    #[test]
    fn native_dropping_pending_play_cancels_producer() {
        let cancelled = Arc::new(AtomicBool::new(false));
        drop(Pending {
            cancelled: cancelled.clone(),
            armed: true,
        });
        assert!(cancelled.load(Ordering::Acquire));
    }
}
