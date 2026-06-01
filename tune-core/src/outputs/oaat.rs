use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::Mutex;
use tracing::info;
#[cfg(feature = "oaat")]
use tracing::{debug, error, warn};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

#[cfg(feature = "oaat")]
enum OaatCommand {
    Pause,
    Resume,
    SetVolume(u8),
    Mute(bool),
}

pub struct OaatOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    controller_id: String,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    current_uri: Arc<Mutex<Option<String>>>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    stop_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(feature = "oaat")]
    command_tx: Mutex<Option<tokio::sync::mpsc::Sender<OaatCommand>>>,
}

impl OaatOutput {
    pub fn new(name: String, host: String, port: u16, endpoint_id: String) -> Self {
        let device_id = if endpoint_id.starts_with("oaat:") {
            endpoint_id
        } else {
            format!("oaat:{endpoint_id}")
        };
        Self {
            name,
            device_id,
            host,
            port,
            controller_id: uuid::Uuid::new_v4().to_string(),
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(800)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(Mutex::new(None)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            stop_tx: Mutex::new(None),
            #[cfg(feature = "oaat")]
            command_tx: Mutex::new(None),
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
        use oaat_core::ChannelLayout;
        use oaat_core::format::AudioFormat;
        use oaat_core::wire::PacketFlags;

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
        let controller_id = self.controller_id.clone();

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tx.lock().await = Some(stop_tx);

        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<OaatCommand>(32);
        *self.command_tx.lock().await = Some(command_tx);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);

        tokio::spawn(async move {
            let config = ControllerConfig {
                controller_id,
                controller_name: "Tune Server".into(),
                features: vec![],
                clock_port: oaat_core::DEFAULT_CLOCK_PORT,
                tls: false,
            };

            // Connect with retry (up to 3 attempts)
            let mut endpoint: Option<ConnectedEndpoint> = None;
            for attempt in 1..=3u32 {
                info!(device = %device_name, addr = %endpoint_addr, attempt, "oaat: connecting to endpoint");
                match ConnectedEndpoint::connect(&config, endpoint_addr).await {
                    Ok(ep) => {
                        info!(device = %device_name, endpoint_name = %ep.info.endpoint_name, "oaat: connected, handshake ok");
                        endpoint = Some(ep);
                        break;
                    }
                    Err(e) => {
                        if attempt < 3 {
                            warn!(device = %device_name, error = %e, attempt, "oaat: connect failed, retrying in 1s");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        } else {
                            error!(device = %device_name, error = %e, "oaat: connect failed after 3 attempts");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
            let mut endpoint = endpoint.unwrap();

            // Clock sync with timeout
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                endpoint.clock_sync_bootstrap(),
            )
            .await
            {
                Ok(Ok(())) => info!(device = %device_name, "oaat: clock sync ok"),
                Ok(Err(e)) => {
                    info!(device = %device_name, error = %e, "oaat: clock sync failed, continuing")
                }
                Err(_) => {
                    info!(device = %device_name, "oaat: clock sync timed out (5s), continuing")
                }
            }

            let stream_id = "tune-stream";

            // Fetch audio stream
            info!(device = %device_name, url = %url, "oaat: fetching audio stream");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let resp = match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    info!(device = %device_name, status = %r.status(), "oaat: stream fetch ok");
                    r
                }
                Ok(r) => {
                    error!(device = %device_name, status = %r.status(), url = %url, "oaat: stream fetch HTTP error");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    error!(device = %device_name, error = %e, url = %url, "oaat: HTTP fetch failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut stream = resp.bytes_stream();
            use futures_util::StreamExt;

            let mut buf = Vec::new();

            while buf.len() < 128 {
                match stream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    _ => {
                        error!(device = %device_name, "oaat: stream ended before header");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }

            // Parse WAV header — reject non-WAV streams
            let (mut sample_rate, channels, mut bits_per_sample) =
                if buf.len() >= 44 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
                    let ch = u16::from_le_bytes([buf[22], buf[23]]);
                    let sr = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
                    let bps = u16::from_le_bytes([buf[34], buf[35]]);

                    let mut data_offset = 12;
                    let mut found_data = false;
                    while data_offset + 8 <= buf.len() {
                        let chunk_id = &buf[data_offset..data_offset + 4];
                        let chunk_size = u32::from_le_bytes([
                            buf[data_offset + 4],
                            buf[data_offset + 5],
                            buf[data_offset + 6],
                            buf[data_offset + 7],
                        ]) as usize;
                        if chunk_id == b"data" {
                            buf.drain(..data_offset + 8);
                            found_data = true;
                            break;
                        }
                        data_offset += 8 + chunk_size;
                    }
                    if !found_data {
                        buf.drain(..44);
                    }
                    (sr, ch, bps)
                } else {
                    let sig: Vec<u8> = buf.iter().take(12).copied().collect();
                    error!(
                        device = %device_name,
                        signature = %format!("{sig:02x?}"),
                        "oaat: stream is not WAV (expected RIFF/WAVE header)"
                    );
                    playing.store(false, Ordering::SeqCst);
                    return;
                };

            let mut format = match bits_per_sample {
                16 => AudioFormat::PcmS16le,
                24 => AudioFormat::PcmS24le,
                32 => AudioFormat::PcmS32le,
                _ => AudioFormat::PcmS16le,
            };
            let mut bytes_per_frame = (bits_per_sample as usize / 8) * channels as usize;
            let samples_per_packet: usize = 480;
            let ch = channels.min(8) as u8;
            let layout = ChannelLayout::Stereo;
            let mut packet_size = samples_per_packet * bytes_per_frame;

            let rate_khz = sample_rate as f64 / 1000.0;
            info!(
                device = %device_name,
                sample_rate, bits_per_sample, channels,
                format = %format,
                "oaat: detected audio format from stream"
            );

            // Format negotiation
            if let Err(e) = endpoint
                .propose_format(
                    stream_id,
                    format,
                    sample_rate,
                    ch,
                    layout,
                    bits_per_sample as u8,
                )
                .await
            {
                error!(device = %device_name, error = %e, "oaat: format propose send failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                endpoint.response_rx.recv(),
            )
            .await
            {
                Ok(Some(oaat_controller::EndpointResponse::FormatAccept(fa))) => {
                    info!(device = %device_name, stream_id = %fa.stream_id, "oaat: format accepted");
                }
                Ok(Some(oaat_controller::EndpointResponse::FormatCounter(fc))) => {
                    info!(
                        device = %device_name,
                        rate = fc.sample_rate,
                        bits = fc.bits_per_sample,
                        format = %fc.format,
                        "oaat: format counter-proposed, adapting"
                    );
                    format = fc.format;
                    bits_per_sample = fc.bits_per_sample as u16;
                    sample_rate = fc.sample_rate;
                    bytes_per_frame = (bits_per_sample as usize / 8) * channels as usize;
                    packet_size = samples_per_packet * bytes_per_frame;
                }
                Ok(Some(oaat_controller::EndpointResponse::FormatReject(fr))) => {
                    error!(device = %device_name, reason = %fr.reason, "oaat: format rejected by endpoint");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Ok(Some(other)) => {
                    warn!(device = %device_name, response = ?other, "oaat: unexpected response to format propose");
                }
                Ok(None) => {
                    error!(device = %device_name, "oaat: endpoint closed connection during format negotiation");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(_) => {
                    error!(device = %device_name, "oaat: format negotiation timed out (5s)");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            }

            // Metadata
            let fmt_str = if rate_khz.fract() == 0.0 {
                format!("PCM {bits_per_sample}/{}", rate_khz as u32)
            } else {
                format!("PCM {bits_per_sample}/{rate_khz:.1}")
            };
            if let Err(e) = endpoint
                .send_metadata(oaat_core::message::TrackMetadata {
                    title,
                    artist,
                    album,
                    duration_ms: 0,
                    artwork_url: None,
                    format: Some(fmt_str),
                })
                .await
            {
                error!(device = %device_name, error = %e, "oaat: send_metadata failed");
            }

            // Play
            if let Err(e) = endpoint.send_play(stream_id).await {
                error!(device = %device_name, error = %e, "oaat: send_play failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }
            info!(device = %device_name, "oaat: play command sent, starting audio stream");

            let mut sample_offset: u64 = 0;
            let mut start = std::time::Instant::now();
            let mut pause_offset = std::time::Duration::ZERO;

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        debug!(device = %device_name, "oaat: stop signal");
                        break;
                    }
                    Some(cmd) = command_rx.recv() => {
                        match cmd {
                            OaatCommand::Pause => {
                                paused.store(true, Ordering::SeqCst);
                                pause_offset = start.elapsed();
                                if let Err(e) = endpoint.send_message(
                                    &oaat_core::Message::Pause(oaat_core::message::Pause {
                                        stream_id: stream_id.to_owned(),
                                    }),
                                ).await {
                                    error!(device = %device_name, error = %e, "oaat: send pause failed");
                                }
                                info!(device = %device_name, "oaat: paused");
                            }
                            OaatCommand::Resume => {
                                paused.store(false, Ordering::SeqCst);
                                // Reset timer so we don't burst-send buffered packets
                                start = std::time::Instant::now() - pause_offset;
                                if let Err(e) = endpoint.send_play(stream_id).await {
                                    error!(device = %device_name, error = %e, "oaat: send resume failed");
                                }
                                info!(device = %device_name, "oaat: resumed");
                            }
                            OaatCommand::SetVolume(level) => {
                                if let Err(e) = endpoint.send_volume(level).await {
                                    error!(device = %device_name, error = %e, "oaat: send volume failed");
                                }
                            }
                            OaatCommand::Mute(muted) => {
                                if let Err(e) = endpoint.send_mute(muted).await {
                                    error!(device = %device_name, error = %e, "oaat: send mute failed");
                                }
                            }
                        }
                    }
                    chunk = stream.next() => {
                        let Some(chunk) = chunk else { break };
                        let chunk = match chunk {
                            Ok(c) => c,
                            Err(e) => {
                                error!(device = %device_name, error = %e, "oaat: stream error");
                                break;
                            }
                        };

                        buf.extend_from_slice(&chunk);

                        while buf.len() >= packet_size
                            && playing.load(Ordering::Relaxed)
                            && !paused.load(Ordering::Relaxed)
                        {
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
            let duration_s = start.elapsed().as_secs_f64();
            let packets = sample_offset / samples_per_packet as u64;
            info!(device = %device_name, samples = sample_offset, packets, duration_s = format!("{duration_s:.1}"), "oaat: playback complete");
        });

        Ok(())
    }

    #[cfg(not(feature = "oaat"))]
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("OAAT support not compiled (enable 'oaat' feature)".into())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Pause).await;
        }
        info!(device = %self.name, "oaat: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Resume).await;
        }
        info!(device = %self.name, "oaat: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        #[cfg(feature = "oaat")]
        {
            self.command_tx.lock().await.take();
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
        let level = (volume.clamp(0.0, 1.0) * 255.0) as u8;
        self.volume
            .store(level as u32, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::SetVolume(level)).await;
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            self.volume.store(0, Ordering::SeqCst);
        }
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Mute(muted)).await;
        }
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                TransportState::Paused
            } else {
                TransportState::Playing
            }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: self.volume.load(Ordering::Relaxed) as f64 / 255.0,
            muted: self.volume.load(Ordering::Relaxed) == 0,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
        })
    }

    async fn is_available(&self) -> bool {
        // mDNS-discovered endpoints are available by definition.
        // Do NOT TCP-probe: a bare connect+close causes the endpoint
        // to see a ghost session ("Disconnected. 0 packets").
        true
    }
}
