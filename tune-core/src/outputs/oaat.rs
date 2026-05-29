use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;
#[cfg(feature = "oaat")]
use tracing::{debug, error};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

pub struct OaatOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    current_uri: Arc<Mutex<Option<String>>>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    stop_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl OaatOutput {
    pub fn new(name: String, host: String, port: u16, endpoint_id: String) -> Self {
        let device_id = format!("oaat:{endpoint_id}");
        Self {
            name,
            device_id,
            host,
            port,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(800)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(Mutex::new(None)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            stop_tx: Mutex::new(None),
        }
    }

    fn endpoint_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.host, self.port).parse().unwrap()
    }
}

#[async_trait::async_trait]
impl OutputTarget for OaatOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "oaat"
    }

    #[cfg(feature = "oaat")]
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        use oaat_controller::{ConnectedEndpoint, ControllerConfig};
        use oaat_core::format::AudioFormat;
        use oaat_core::wire::PacketFlags;
        use oaat_core::ChannelLayout;

        self.stop().await.ok();

        let url = media.url.to_owned();
        let title = media.title.unwrap_or("Unknown").to_owned();
        let artist = media.artist.unwrap_or("Unknown").to_owned();
        let album = media.album.unwrap_or("").to_owned();

        *self.current_uri.lock().await = Some(url.clone());
        *self.current_title.lock().await = Some(title.clone());
        *self.current_artist.lock().await = Some(artist.clone());

        info!(device = %self.name, url = %url, title = %title, "oaat: play_media");

        let endpoint_addr = self.endpoint_addr();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let position_ms = self.position_ms.clone();
        let device_name = self.name.clone();

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tx.lock().await = Some(stop_tx);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);

        tokio::spawn(async move {
            let config = ControllerConfig {
                controller_id: uuid::Uuid::new_v4().to_string(),
                controller_name: "Tune Server".into(),
                features: vec![],
                clock_port: oaat_core::DEFAULT_CLOCK_PORT,
            };

            let mut endpoint = match ConnectedEndpoint::connect(&config, endpoint_addr).await {
                Ok(ep) => ep,
                Err(e) => {
                    error!(device = %device_name, error = %e, "oaat: connect failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            if let Err(e) = endpoint.clock_sync_bootstrap().await {
                debug!(error = %e, "oaat: clock sync failed, continuing");
            }

            let sample_rate = 44100u32;
            let format = AudioFormat::PcmS16le;
            let stream_id = "tune-stream";

            if let Err(e) = endpoint
                .propose_format(stream_id, format, sample_rate, 2, ChannelLayout::Stereo, 16)
                .await
            {
                error!(error = %e, "oaat: format propose failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            endpoint
                .send_metadata(oaat_core::message::TrackMetadata {
                    title,
                    artist,
                    album,
                    duration_ms: 0,
                    artwork_url: None,
                    format: Some("PCM 16/44".into()),
                })
                .await
                .ok();

            endpoint.send_play(stream_id).await.ok();

            let client = reqwest::Client::new();
            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "oaat: HTTP fetch failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut stream = resp.bytes_stream();
            use futures_util::StreamExt;

            let mut buf = Vec::new();
            let mut wav_header_skipped = false;
            let mut sample_offset: u64 = 0;
            let bytes_per_frame = 4usize; // 16-bit stereo
            let samples_per_packet = 480usize;
            let packet_size = samples_per_packet * bytes_per_frame;
            let start = std::time::Instant::now();

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        debug!("oaat: stop signal");
                        break;
                    }
                    chunk = stream.next() => {
                        let Some(chunk) = chunk else { break };
                        let chunk = match chunk {
                            Ok(c) => c,
                            Err(e) => {
                                error!(error = %e, "oaat: stream error");
                                break;
                            }
                        };

                        buf.extend_from_slice(&chunk);

                        if !wav_header_skipped && buf.len() >= 44 {
                            if &buf[..4] == b"RIFF" {
                                buf.drain(..44);
                            }
                            wav_header_skipped = true;
                        }

                        if !wav_header_skipped { continue; }

                        while buf.len() >= packet_size && playing.load(Ordering::Relaxed) {
                            while paused.load(Ordering::Relaxed) {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                if !playing.load(Ordering::Relaxed) { break; }
                            }

                            let payload: Vec<u8> = buf.drain(..packet_size).collect();
                            let pts_ns = (sample_offset as f64 / sample_rate as f64 * 1e9) as u64;
                            let flags = if sample_offset == 0 {
                                PacketFlags::FIRST_PACKET
                            } else {
                                PacketFlags::empty()
                            };

                            if endpoint.send_audio(1, format, pts_ns, sample_offset, &payload, flags).await.is_err() {
                                break;
                            }

                            sample_offset += samples_per_packet as u64;
                            position_ms.store(sample_offset * 1000 / sample_rate as u64, Ordering::Relaxed);

                            let expected = std::time::Duration::from_nanos(
                                (sample_offset as f64 / sample_rate as f64 * 1e9) as u64,
                            );
                            let elapsed = start.elapsed();
                            if expected > elapsed {
                                tokio::time::sleep(expected - elapsed).await;
                            }
                        }
                    }
                }
            }

            endpoint.send_stop(stream_id).await.ok();
            playing.store(false, Ordering::SeqCst);
            info!(device = %device_name, samples = sample_offset, "oaat: complete");
        });

        Ok(())
    }

    #[cfg(not(feature = "oaat"))]
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("OAAT support not compiled (enable 'oaat' feature)".into())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        info!(device = %self.name, "oaat: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "oaat: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        *self.current_uri.lock().await = None;
        info!(device = %self.name, "oaat: stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.volume.store((volume.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::SeqCst);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted { self.volume.store(0, Ordering::SeqCst); }
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) { TransportState::Paused }
            else { TransportState::Playing }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: self.volume.load(Ordering::Relaxed) == 0,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
        })
    }

    async fn is_available(&self) -> bool {
        tokio::net::TcpStream::connect(self.endpoint_addr()).await.is_ok()
    }
}
