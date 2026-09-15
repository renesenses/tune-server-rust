//! Disposable decoder process -> standard bounded PCM source.
use super::{
    SpotifyNativeService,
    engine::SpotifyNativeService as Engine,
    ipc::{self, ChildProcess, Failure, Operation},
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
use tokio::io::AsyncReadExt;
use tune_core::streaming::{
    audio_source::{DecodedPcmSource, PcmFormat as SourcePcmFormat, PcmRequest, PcmSender},
    traits::*,
};

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

impl AudioHeader {
    /// Trust only the decoder's confirmed codec/PCM pair, not the requested
    /// quality. The returned source format must survive the WAV transport.
    fn confirmed_source_format(
        &self,
        requested: Option<i32>,
    ) -> Result<tune_core::audio::formats::AudioFormat, String> {
        use tune_core::audio::formats::AudioFormat;
        self.pcm.validate()?;
        match (requested, self.source_codec.as_str()) {
            (None, "vorbis") if self.pcm == PcmFormat::ogg() => Ok(AudioFormat::Ogg),
            (Some(16), "flac") if self.pcm.bit_depth == 16 => Ok(AudioFormat::Flac),
            (Some(22), "flac") if self.pcm.bit_depth == 24 => Ok(AudioFormat::Flac),
            _ => Err("Spotify worker did not decode the requested quality".into()),
        }
    }
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

async fn cancelled(cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn forward_pcm(
    mut child: ChildProcess,
    tx: &PcmSender,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut buffer = [0u8; 4096];
    let produce = async {
        loop {
            let size = child
                .output
                .read(&mut buffer)
                .await
                .map_err(|_| "Spotify PCM pipe failed")?;
            if size == 0 {
                return child.successful_exit().await;
            }
            tx.send(buffer[..size].to_vec()).await?;
        }
    };
    // Covers both a child blocked writing to a full pipe and a child stuck
    // reading upstream. Dropping ChildProcess signals kill + supervisor reap.
    tokio::select! {
        result = produce => result,
        _ = cancelled(cancel) => Err("Spotify playback cancelled".into()),
        _ = tx.closed() => Ok(()),
    }
}

impl SpotifyNativeService {
    pub async fn open_audio(&self, req: &PcmRequest<'_>) -> Result<DecodedPcmSource, String> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        let id = req.source_id;
        super::catalog::uri(id, "track").map_err(|e| e.to_string())?;
        if !self.has_credentials() {
            return Err("Spotify: pair Tune from the Spotify app first".into());
        }
        let seek_ms =
            u32::try_from(req.seek_ms).map_err(|_| "Spotify seek exceeds supported range")?;
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
        let source_format = header.confirmed_source_format(lossless_format)?;
        let (tx, source) = DecodedPcmSource::channel(
            SourcePcmFormat {
                sample_rate: pcm.sample_rate,
                bit_depth: pcm.bit_depth,
                channels: pcm.channels,
            },
            source_format,
            header.track,
        )?;
        let cancel_task = cancel.clone();
        let active = self.audio.clone();
        tokio::spawn(async move {
            let result = forward_pcm(child, &tx, &cancel_task).await;
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
            if let Err(error) = result {
                // Core owns readiness, HTTP lifetime and error teardown.
                tx.fail(error).await;
            }
        });
        pending.armed = false;
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn native_core_pcm_drop_stops_worker_even_with_blocked_pipe() {
        use tokio::io::AsyncBufReadExt;
        for script in ["sleep 30", "exec yes pcm"] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", &format!("printf '%s\\n' \"$$\"; {script}")]);
            let mut child = ChildProcess::from_command(command).unwrap();
            let mut pid = String::new();
            tokio::time::timeout(Duration::from_secs(2), child.output.read_line(&mut pid))
                .await
                .unwrap()
                .unwrap();
            let pid: i32 = pid.trim().parse().unwrap();
            let life = child.life.clone();
            let (tx, source) = DecodedPcmSource::channel(
                SourcePcmFormat {
                    sample_rate: 44100,
                    bit_depth: 16,
                    channels: 2,
                },
                tune_core::audio::formats::AudioFormat::Ogg,
                serde_json::from_value(serde_json::json!({
                    "id": "fixture", "title": "Fixture", "artist": "Fixture",
                    "duration_ms": 1000, "explicit": false
                }))
                .unwrap(),
            )
            .unwrap();
            let task =
                tokio::spawn(async move { forward_pcm(child, &tx, &AtomicBool::new(false)).await });
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(life.load(Ordering::Acquire));
            drop(source);
            let result = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("core dropping PCM must stop a provider blocked on IO or backpressure")
                .unwrap();
            // send() and closed() can both become ready on the same drop.
            if let Err(error) = result {
                assert_eq!(error, "PCM consumer closed");
            }
            assert!(!life.load(Ordering::Acquire));
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    // Includes zombies: kill(0) only fails with ESRCH once the
                    // exact child is gone, not just when Drop sets life=false.
                    if unsafe { libc::kill(pid, 0) } == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("provider process must actually exit and be reaped");
        }
    }
    #[test]
    fn native_confirmed_source_comes_from_decoder_not_quality_preference() {
        use tune_core::audio::formats::AudioFormat;
        let mut header = AudioHeader {
            track: serde_json::from_value(serde_json::json!({
                "id": "fixture", "title": "Fixture", "artist": "Fixture",
                "duration_ms": 1000, "explicit": false
            }))
            .unwrap(),
            pcm: PcmFormat::ogg(),
            source_codec: "vorbis".into(),
        };
        assert_eq!(
            header.confirmed_source_format(None).unwrap(),
            AudioFormat::Ogg
        );
        assert!(
            header.confirmed_source_format(Some(16)).is_err(),
            "requesting FLAC must not relabel a Vorbis decoder"
        );
        header.source_codec = "flac".into();
        assert_eq!(
            header.confirmed_source_format(Some(16)).unwrap(),
            AudioFormat::Flac
        );
        assert!(header.confirmed_source_format(None).is_err());
        assert!(header.confirmed_source_format(Some(22)).is_err());
        header.pcm.bit_depth = 24;
        assert_eq!(
            header.confirmed_source_format(Some(22)).unwrap(),
            AudioFormat::Flac
        );
        header.source_codec = "unknown".into();
        assert!(header.confirmed_source_format(Some(22)).is_err());
    }
    #[test]
    fn native_pcm_is_little_endian_saturating_and_nan_safe() {
        assert_eq!(
            pcm_s16le(&[-1.0, 0.5, 1.0, -2.0, f64::NAN]),
            [0, 128, 0, 64, 255, 127, 0, 128, 0, 0]
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
