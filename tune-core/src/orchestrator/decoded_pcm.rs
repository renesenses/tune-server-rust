//! Host adapter for every provider implementing DecodedPcm, not Spotify-specific.
use super::*;
use crate::audio::tap::{RawWindow, WINDOW_MS};
use crate::streaming::audio_source::{DecodedPcmSource, PcmFormat};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const LEVELS_WINDOWS: usize = 30_000 / WINDOW_MS as usize;

fn stream_info(format: PcmFormat) -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: format.sample_rate,
        bit_depth: format.bit_depth,
        channels: format.channels,
        // Catalogue duration is not an exact frame count. Finite chunked HTTP
        // ends on producer EOF, never an invented Content-Length.
        duration_ms: None,
        ..Default::default()
    }
}

struct PendingSession(Arc<AtomicBool>);
impl Drop for PendingSession {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl PlaybackOrchestrator {
    pub(super) async fn serve_decoded_pcm(
        &self,
        service_name: &str,
        mut source: DecodedPcmSource,
        play_seq: u64,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let format = source.format;
        format.validate()?;
        let seek_ms = req.seek_ms.unwrap_or(0);
        let start_ms = i64::try_from(seek_ms).map_err(|_| "PCM seek exceeds supported range")?;
        // Check with the actual decoder format too (not catalogue metadata).
        if self
            .load_streaming_dsp(
                req.zone_id,
                req.track_id,
                format.sample_rate,
                format.channels,
            )
            .is_active()
        {
            return Err("Decoded PCM delivery does not yet apply zone DSP".into());
        }
        let zone_id = req.zone_id;
        // The caller captured ownership BEFORE awaiting the provider. A slow
        // old decoder must not attach its spectrum to a newer play request.
        if self.playback.current_play_seq(zone_id).await != play_seq {
            return Err("Decoded PCM source was superseded".into());
        }
        let levels_cancelled = Arc::new(AtomicBool::new(false));
        let mut levels = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))
            .map(|bus| {
                let (tx, rx) = mpsc::channel(LEVELS_WINDOWS);
                spawn_levels_receiver(
                    bus,
                    self.playback.clone(),
                    zone_id,
                    play_seq,
                    start_ms,
                    LevelsReceiver::Bounded {
                        rx,
                        cancelled: levels_cancelled.clone(),
                    },
                );
                tx
            });
        let (stream_id, tx, ready) = self
            .streamer
            .create_session(stream_info(format), false, 64)
            .await;
        if let Some(session) = self.streamer.sessions_state().lock().await.get(&stream_id) {
            let _ = session.restart_position_ms.set(seek_ms);
            let _ = session.decoded_source_format.set(source.source_format);
        }
        let aborted = Arc::new(AtomicBool::new(false));
        // If resolution is dropped while waiting for the first PCM, the task
        // must close the producer and remove its otherwise orphaned session.
        let pending = PendingSession(aborted.clone());
        let committed = Arc::new(AtomicBool::new(false));
        let task_committed = committed.clone();
        let (started, startup) = oneshot::channel();
        let streamer = self.streamer.clone();
        let task_id = stream_id.clone();
        let playback = self.playback.clone();
        tokio::spawn(async move {
            let cancelled = async {
                loop {
                    if (!task_committed.load(Ordering::Acquire) && aborted.load(Ordering::Acquire))
                        || !streamer.session_alive(&task_id).await
                        || playback.current_play_seq(zone_id).await != play_seq
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            };
            let mut started = Some(started);
            let frame_bytes = format.frame_bytes();
            let window_bytes =
                (u64::from(format.sample_rate) * WINDOW_MS / 1000) as usize * frame_bytes;
            let feed = async {
                let mut carry = Vec::with_capacity(window_bytes);
                // Reassemble exact 40 ms windows across arbitrary provider reads.
                // HTTP and analysis see identical signed PCM bytes, untouched.
                while let Some(chunk) = source.pcm.recv().await {
                    carry.extend_from_slice(&chunk?);
                    while carry.len() >= window_bytes {
                        let tail = carry.split_off(window_bytes);
                        let pcm = std::mem::replace(&mut carry, tail);
                        tx.send(pcm.clone())
                            .await
                            .map_err(|_| "PCM output closed")?;
                        if let Some(started) = started.take() {
                            ready.notify_one();
                            let _ = started.send(Ok::<_, String>(()));
                        }
                        send_levels(&mut levels, pcm, format).await;
                    }
                }
                if carry.len() % frame_bytes != 0 {
                    return Err("Decoded PCM ended inside an audio frame".to_string());
                }
                if !carry.is_empty() {
                    tx.send(carry.clone())
                        .await
                        .map_err(|_| "PCM output closed")?;
                    if let Some(started) = started.take() {
                        ready.notify_one();
                        let _ = started.send(Ok(()));
                    }
                    send_levels(&mut levels, carry, format).await;
                }
                if started.is_some() {
                    return Err("Decoder produced no PCM".into());
                }
                Ok::<_, String>(())
            };
            let result = tokio::select! {
                result = feed => result,
                _ = cancelled => Err("Decoded PCM playback cancelled".into()),
            };
            // Dropping this receiver is the provider's cancellation contract.
            drop(source.pcm);
            drop(tx);
            drop(levels);
            streamer.end_session_input(&task_id).await;
            if let Some(started) = started {
                let _ = started.send(Err(result
                    .clone()
                    .err()
                    .unwrap_or_else(|| "Decoder produced no PCM".into())));
            }
            if result.is_err() {
                levels_cancelled.store(true, Ordering::Release);
                tracing::debug!(stream_id = %task_id, "decoded_pcm_producer_ended_early");
                streamer.remove_session(&task_id).await;
            }
        });
        let result = tokio::time::timeout(Duration::from_secs(30), startup)
            .await
            .map_err(|_| "PCM decoder startup timed out".to_string())
            .and_then(|r| r.map_err(|_| "PCM decoder stopped before startup".to_string()))
            .and_then(|r| r);
        if let Err(error) = result {
            self.streamer.remove_session(&stream_id).await;
            return Err(error);
        }
        committed.store(true, Ordering::Release);
        drop(pending);
        let track = source.track;
        Ok(ResolvedStream {
            url: self
                .streamer
                .get_stream_url(&stream_id, &self.server_ip(), "wav"),
            mime_type: "audio/wav".into(),
            title: track.title,
            artist: Some(track.artist),
            album: track.album,
            duration_ms: Some(track.duration_ms as i64),
            source: service_name.into(),
            cover_url: track.cover_path,
            stream_id: Some(stream_id),
            file_size: None,
            sample_rate: Some(format.sample_rate),
            bit_depth: Some(u32::from(format.bit_depth)),
            channels: Some(u32::from(format.channels)),
            origin_url: None,
            bitrate_kbps: None,
        })
    }
}

async fn send_levels(
    levels: &mut Option<mpsc::Sender<RawWindow>>,
    pcm: Vec<u8>,
    format: PcmFormat,
) {
    if let Some(tx) = levels {
        let frames = pcm.len() / format.frame_bytes();
        if tx
            .send(RawWindow {
                pcm,
                bit_depth: format.bit_depth,
                channels: format.channels,
                sample_rate: format.sample_rate,
                window: Duration::from_secs_f64(frames as f64 / f64::from(format.sample_rate)),
            })
            .await
            .is_err()
        {
            // A visualizer lifetime must not turn successful PCM into an error.
            *levels = None;
        }
    }
}

#[cfg(test)]
mod tests;
