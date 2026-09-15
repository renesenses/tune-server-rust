//! Disposable decoder process -> bounded PCM -> Tune HTTP session.
use super::{
    SpotifyNativeService,
    engine::SpotifyNativeService as Engine,
    ipc::{self, ChildProcess, Failure, Operation},
};
use crate::{
    http::streamer::{AudioStreamer, StreamInfo},
    orchestrator::{PlayRequest, ResolvedStream},
    streaming::traits::*,
};
use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    config::{Bitrate, PlayerConfig},
    convert::Converter,
    decoder::AudioPacket,
    mixer::NoOpVolume,
    player::{Player, PlayerEvent},
};
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::{io::AsyncReadExt, sync::oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PcmFormat {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}
impl PcmFormat {
    pub fn validate(self) -> Result<(), String> {
        if !(8000..=192000).contains(&self.sample_rate)
            || ![16, 24].contains(&self.bit_depth)
            || ![1, 2].contains(&self.channels)
        {
            return Err("Spotify worker returned unsupported PCM parameters".into());
        }
        Ok(())
    }
    pub fn frame_bytes(self) -> usize {
        usize::from(self.channels) * usize::from(self.bit_depth / 8)
    }
    fn ogg() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            bit_depth: 16,
            channels: NUM_CHANNELS as u16,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct AudioHeader {
    // Preserve existing diagnostic readers of the private track header.
    #[serde(flatten)]
    pub track: StreamTrack,
    pub pcm: PcmFormat,
    pub source_codec: String,
}

pub(super) struct Lease {
    zone: i64,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
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

fn pcm_s16le(samples: &[f64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let sample = if sample.is_finite() { *sample } else { 0.0 };
        bytes.extend_from_slice(&((sample * 32768.0).round() as i16).to_le_bytes());
    }
    bytes
}

// Constructed exclusively by run_audio_worker, never by the server.
struct PipeSink;
impl Sink for PipeSink {
    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        let AudioPacket::Samples(samples) = packet else {
            return Err(SinkError::InvalidParams(
                "Spotify packets are not PCM".into(),
            ));
        };
        if samples.len() % NUM_CHANNELS as usize != 0 {
            return Err(SinkError::InvalidParams(
                "Spotify PCM is not frame aligned".into(),
            ));
        }
        let mut stdout = std::io::stdout();
        for chunk in samples.chunks(2048) {
            // Blocking is intentional on librespot's dedicated audio thread.
            // The parent kills/reaps this CHILD on stop, even if the pipe is full.
            stdout
                .write_all(&pcm_s16le(chunk))
                .map_err(|_| SinkError::NotConnected("Tune PCM pipe closed".into()))?;
        }
        stdout
            .flush()
            .map_err(|_| SinkError::NotConnected("Tune PCM pipe closed".into()))
    }
}

pub(super) async fn run_audio_worker() -> Result<(), String> {
    let published = Arc::new(AtomicBool::new(false));
    let result = run_audio_worker_inner(published.clone()).await;
    if let Err(error) = &result {
        if !published.load(Ordering::Acquire) {
            // Refusals before PCM are meaningful startup replies, not opaque EOF.
            let reply: Result<AudioHeader, Failure> = Err(Failure::from_tune(error.clone().into()));
            let _ = ipc::write_frame(&mut tokio::io::stdout(), &reply).await;
        }
    }
    result
}

async fn run_audio_worker_inner(published: Arc<AtomicBool>) -> Result<(), String> {
    let Operation::Play {
        tokens,
        id,
        seek_ms,
        lossless_format,
    } = ipc::read_frame(&mut tokio::io::stdin()).await?
    else {
        return Err("Audio worker expected Play".into());
    };
    let mut engine = Engine::new();
    if !engine.restore_tokens(&tokens) {
        return Err("Spotify credentials are missing".into());
    }
    let session = engine.session().await.map_err(|e| e.to_string())?;
    session
        .spclient()
        .set_strategy(librespot_core::spclient::RequestStrategy::TryTimes(1));
    let track = super::catalog::track(&session, &id)
        .await
        .map_err(|e| e.to_string())?;
    if track.duration_ms == 0 || seek_ms as u64 >= track.duration_ms {
        return Err("Spotify seek is out of range".into());
    }
    if let Some(format) = lossless_format {
        let prepared = super::lossless::prepare(&session, &id, format).await?;
        return tokio::task::spawn_blocking(move || prepared.decode(seek_ms, track, published))
            .await
            .map_err(|_| "Spotify FLAC decoder task failed")?;
    }
    let uri = super::catalog::uri(&id, "track").map_err(|e| e.to_string())?;
    let header: Result<AudioHeader, Failure> = Ok(AudioHeader {
        track,
        pcm: PcmFormat::ogg(),
        source_codec: "vorbis".into(),
    });
    ipc::write_frame(&mut tokio::io::stdout(), &header).await?;
    published.store(true, Ordering::Release);
    let player = Player::new(
        PlayerConfig {
            bitrate: Bitrate::Bitrate320,
            normalisation: false,
            gapless: false,
            ..Default::default()
        },
        session,
        Box::new(NoOpVolume),
        || Box::new(PipeSink),
    );
    let mut events = player.get_player_event_channel();
    player.load(uri, true, seek_ms);
    let result = loop {
        match events.recv().await {
            Some(PlayerEvent::EndOfTrack { .. }) => break Ok(()),
            Some(PlayerEvent::Unavailable { .. }) => break Err("Spotify track unavailable".into()),
            None => break Err("Spotify decoder stopped unexpectedly".into()),
            _ => {}
        }
    };
    player.stop();
    drop(player);
    result
}

fn pcm_stream_info(pcm: PcmFormat) -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: pcm.sample_rate,
        bit_depth: pcm.bit_depth,
        channels: pcm.channels,
        // Spotify metadata milliseconds are NOT an exact decoded frame count.
        // An invented Content-Length can leave a renderer waiting after EOF.
        // HTTP is chunked; the child pipe's EOF terminates this finite stream.
        duration_ms: None,
        ..Default::default()
    }
}

async fn cancelled(cancel: &AtomicBool, streamer: &AudioStreamer, id: &str) {
    loop {
        if cancel.load(Ordering::Acquire) || !streamer.session_alive(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

impl SpotifyNativeService {
    pub async fn resolve_audio(
        &self,
        streamer: Arc<AudioStreamer>,
        server_ip: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        let id = req
            .source_id
            .as_deref()
            .ok_or("Spotify track id is required")?;
        super::catalog::uri(id, "track").map_err(|e| e.to_string())?;
        if !self.has_credentials() {
            return Err("Spotify: pair Tune from the Spotify app first".into());
        }
        let seek_ms = u32::try_from(req.seek_ms.unwrap_or(0))
            .map_err(|_| "Spotify seek exceeds supported range")?;
        let lossless_format = super::lossless::requested_format()?;
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.audio.lock().unwrap();
            if active.as_ref().is_some_and(|lease| {
                lease.zone != req.zone_id && !lease.cancelled.load(Ordering::Acquire)
            }) {
                return Err(
                    "Spotify native permits one active zone; stop the other zone first".into(),
                );
            }
            *active = Some(Lease {
                zone: req.zone_id,
                cancelled: cancel.clone(),
            });
        }
        let mut pending = Pending {
            cancelled: cancel.clone(),
            armed: true,
        };
        let mut child = ChildProcess::spawn("audio").map_err(|e| e.to_string())?;
        let header: Result<AudioHeader, Failure> = tokio::time::timeout(ipc::DEADLINE, async {
            ipc::write_frame(
                &mut child.input,
                &Operation::Play {
                    tokens: self.client.tokens(),
                    id: id.into(),
                    seek_ms,
                    lossless_format,
                },
            )
            .await?;
            ipc::read_frame(&mut child.output).await
        })
        .await
        .map_err(|_| "Spotify audio worker startup timed out")??;
        let header = header.map_err(|e| e.into_tune().to_string())?;
        let pcm = header.pcm;
        pcm.validate()?;
        match (lossless_format, header.source_codec.as_str()) {
            (None, "vorbis") if pcm == PcmFormat::ogg() => {}
            (Some(16), "flac") if pcm.bit_depth == 16 => {}
            (Some(22), "flac") if pcm.bit_depth == 24 => {}
            _ => return Err("Spotify worker did not decode the requested quality".into()),
        }
        let track = header.track;
        let frame_bytes = pcm.frame_bytes();
        let (stream_id, tx, ready) = streamer
            .create_session(pcm_stream_info(pcm), false, 64)
            .await;
        if let Some(session) = streamer.sessions_state().lock().await.get(&stream_id) {
            let _ = session.restart_position_ms.set(u64::from(seek_ms));
        }
        let (started, mut startup) = oneshot::channel::<Result<(), String>>();
        let stream_task = streamer.clone();
        let task_id = stream_id.clone();
        let cancel_task = cancel.clone();
        let active = self.audio.clone();
        tokio::spawn(async move {
            let mut started = Some(started);
            let mut buffer = [0u8; 4096];
            let mut carry = Vec::with_capacity(4100);
            let result: Result<(), String> = async {
                loop {
                    let size = tokio::select! {
                        _ = cancelled(&cancel_task, &stream_task, &task_id) => return Err("Spotify playback cancelled".into()),
                        result = child.output.read(&mut buffer) => result.map_err(|_| "Spotify PCM pipe failed")?,
                    };
                    if size == 0 {
                        if !carry.is_empty() { return Err("Spotify PCM ended inside an audio frame".into()); }
                        return child.successful_exit().await;
                    }
                    carry.extend_from_slice(&buffer[..size]);
                    let length = carry.len() / frame_bytes * frame_bytes;
                    if length == 0 { continue; }
                    let tail = carry.split_off(length);
                    let chunk = std::mem::replace(&mut carry, tail);
                    tokio::select! {
                        _ = cancelled(&cancel_task, &stream_task, &task_id) => return Err("Spotify playback cancelled".into()),
                        result = tx.send(chunk) => result.map_err(|_| "Tune PCM consumer closed")?,
                    }
                    if let Some(started) = started.take() { ready.notify_one(); let _ = started.send(Ok(())); }
                }
            }.await;
            if let Some(started) = started {
                let _ = started.send(Err(result
                    .clone()
                    .err()
                    .unwrap_or_else(|| "Spotify decoder produced no PCM".into())));
            }
            drop(child); // signal, kill if still alive, and reap in the supervisor
            drop(tx);
            stream_task.end_session_input(&task_id).await;
            cancel_task.store(true, Ordering::Release);
            {
                let mut current = active.lock().unwrap();
                if current
                    .as_ref()
                    .is_some_and(|lease| Arc::ptr_eq(&lease.cancelled, &cancel_task))
                {
                    current.take();
                }
            }
            if result.is_err() {
                tracing::warn!(stream_id = %task_id, "spotify_native_producer_ended_early");
                stream_task.remove_session(&task_id).await;
            }
        });
        let result = tokio::time::timeout(Duration::from_secs(30), &mut startup)
            .await
            .map_err(|_| "Spotify decoder startup timed out".to_owned())
            .and_then(|r| r.map_err(|_| "Spotify decoder stopped before producing PCM".to_owned()))
            .and_then(|r| r);
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
            sample_rate: Some(pcm.sample_rate),
            bit_depth: Some(u32::from(pcm.bit_depth)),
            channels: Some(u32::from(pcm.channels)),
            origin_url: None,
            bitrate_kbps: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pcm_is_little_endian_saturating_and_nan_safe() {
        assert_eq!(
            pcm_s16le(&[-1.0, 0.5, 1.0, -2.0, f64::NAN]),
            [0, 128, 0, 64, 255, 127, 0, 128, 0, 0]
        );
    }
    #[test]
    fn native_pcm_never_invents_content_length_from_catalogue_duration() {
        assert_eq!(
            pcm_stream_info(PcmFormat::ogg()).wav_content_length(),
            None,
            "Spotify metadata duration is not an exact PCM length; Chrome must receive real EOF"
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
