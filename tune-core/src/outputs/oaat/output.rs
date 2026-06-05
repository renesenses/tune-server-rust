use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::Mutex;
#[cfg(feature = "oaat")]
use tracing::{debug, error, info, warn};

use crate::outputs::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

#[cfg(feature = "oaat")]
use super::helpers::{
    StreamInfo, detect_and_parse, dsd_rate_from_sample_rate, format_rate_display,
};

#[cfg(feature = "oaat")]
enum OaatCommand {
    Pause,
    Resume,
    SetVolume(u8),
    Mute(bool),
    Seek {
        position_ms: u64,
    },
    PrepareNext {
        url: String,
        title: String,
        artist: String,
        album: String,
        cover_url: Option<String>,
        duration_ms: u64,
    },
}

#[cfg(feature = "oaat")]
struct NextTrackPrefetch {
    stream: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    buf: Vec<u8>,
    info: StreamInfo,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
    same_format: bool,
}

/// Observable OAAT diagnostics — safe to read from any thread.
#[derive(Default)]
pub struct OaatDiagnostics {
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub reconnects: AtomicU32,
    pub last_packet_epoch_ms: AtomicU64,
    pub format_desc: std::sync::Mutex<String>,
    pub connected: AtomicBool,
    pub is_flac: AtomicBool,
}

pub struct OaatOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    controller_id: String,
    stream_counter: Arc<AtomicU32>,
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
    pub diag: Arc<OaatDiagnostics>,
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
            stream_counter: Arc::new(AtomicU32::new(1)),
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
            diag: Arc::new(OaatDiagnostics::default()),
        }
    }

    pub fn diagnostics_snapshot(&self) -> serde_json::Value {
        let playing = self.playing.load(Ordering::Relaxed);
        let last_pkt = self.diag.last_packet_epoch_ms.load(Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let stale_ms = if last_pkt > 0 && playing {
            now_ms.saturating_sub(last_pkt)
        } else {
            0
        };

        serde_json::json!({
            "device_id": self.device_id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "controller_id": self.controller_id,
            "connected": self.diag.connected.load(Ordering::Relaxed),
            "playing": playing,
            "paused": self.paused.load(Ordering::Relaxed),
            "is_flac": self.diag.is_flac.load(Ordering::Relaxed),
            "format": *self.diag.format_desc.lock().unwrap(),
            "packets_sent": self.diag.packets_sent.load(Ordering::Relaxed),
            "bytes_sent": self.diag.bytes_sent.load(Ordering::Relaxed),
            "reconnects": self.diag.reconnects.load(Ordering::Relaxed),
            "position_ms": self.position_ms.load(Ordering::Relaxed),
            "duration_ms": self.duration_ms.load(Ordering::Relaxed),
            "last_packet_age_ms": stale_ms,
            "stall_detected": playing && !self.paused.load(Ordering::Relaxed) && stale_ms > 5000,
        })
    }

    fn endpoint_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.host, self.port).parse().unwrap()
    }
}

#[cfg(feature = "oaat")]
const FLAC_CHUNK_SIZE: usize = 4096;
#[cfg(feature = "oaat")]
const DSD_CHUNK_SIZE: usize = 4096;
#[cfg(feature = "oaat")]
const PCM_SAMPLES_PER_PACKET: usize = 480;
#[cfg(feature = "oaat")]
const MAX_RECONNECT_ATTEMPTS: u32 = 2;

#[cfg(feature = "oaat")]
async fn connect_and_setup(
    config: &oaat_controller::ControllerConfig,
    endpoint_addr: std::net::SocketAddr,
    device_name: &str,
    stream_id: &str,
    stream_info: &StreamInfo,
) -> Option<oaat_controller::ConnectedEndpoint> {
    use oaat_core::ChannelLayout;

    let mut endpoint = match oaat_controller::ConnectedEndpoint::connect(config, endpoint_addr)
        .await
    {
        Ok(ep) => {
            info!(device = %device_name, endpoint_name = %ep.info.endpoint_name, "oaat: reconnected");
            ep
        }
        Err(e) => {
            error!(device = %device_name, error = %e, "oaat: reconnect failed");
            return None;
        }
    };

    // Quick clock sync (2 exchanges instead of full 10)
    match tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for seq in 0..2u16 {
            let _ = endpoint.clock_sync_once(seq).await;
        }
    })
    .await
    {
        Ok(()) => {}
        Err(_) => info!(device = %device_name, "oaat: reconnect clock sync skipped (timeout)"),
    }

    let ch = stream_info.channels.min(8) as u8;
    if let Err(e) = endpoint
        .propose_format(
            stream_id,
            stream_info.format,
            stream_info.sample_rate,
            ch,
            ChannelLayout::Stereo,
            stream_info.bits_per_sample as u8,
        )
        .await
    {
        error!(device = %device_name, error = %e, "oaat: reconnect format propose failed");
        return None;
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        endpoint.response_rx.recv(),
    )
    .await
    {
        Ok(Some(oaat_controller::EndpointResponse::FormatAccept(_))) => {}
        Ok(Some(oaat_controller::EndpointResponse::FormatCounter(_))) => {}
        _ => {
            error!(device = %device_name, "oaat: reconnect format negotiation failed");
            return None;
        }
    }

    if let Err(e) = endpoint.send_play(stream_id).await {
        error!(device = %device_name, error = %e, "oaat: reconnect play failed");
        return None;
    }

    info!(device = %device_name, "oaat: reconnected and resumed");
    Some(endpoint)
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
        let file_path = media.file_path.map(|s| s.to_owned());
        let title = media.title.unwrap_or("Unknown").to_owned();
        let artist = media.artist.unwrap_or("Unknown").to_owned();
        let album = media.album.unwrap_or("").to_owned();
        let cover_url = media.cover_url.map(|s| s.to_owned());
        let track_duration_ms = media.duration_ms.unwrap_or(0);

        *self.current_uri.lock().await = Some(url.clone());
        *self.current_title.lock().await = Some(title.clone());
        *self.current_artist.lock().await = Some(artist.clone());
        self.duration_ms.store(track_duration_ms, Ordering::SeqCst);

        info!(device = %self.name, url = %url, title = %title, "oaat: play_media");

        let endpoint_addr = self.endpoint_addr();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let position_ms = self.position_ms.clone();
        let duration_ms_arc = self.duration_ms.clone();
        let current_title = self.current_title.clone();
        let current_artist = self.current_artist.clone();
        let current_uri = self.current_uri.clone();
        let diag = self.diag.clone();
        let device_name = self.name.clone();
        let controller_id = self.controller_id.clone();
        let stream_num = self.stream_counter.fetch_add(1, Ordering::SeqCst);

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tx.lock().await = Some(stop_tx);

        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<OaatCommand>(32);
        *self.command_tx.lock().await = Some(command_tx);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);

        tokio::spawn(async move {
            use futures_util::StreamExt;

            debug!(device = %device_name, url = %url, "oaat: play_media spawned");

            let config = ControllerConfig {
                controller_id,
                controller_name: "Tune Server".into(),
                features: vec![],
                clock_port: oaat_core::DEFAULT_CLOCK_PORT,
                tls: false,
            };

            // Connect with retry
            let mut endpoint: Option<ConnectedEndpoint> = None;
            for attempt in 1..=3u32 {
                debug!(device = %device_name, addr = %endpoint_addr, attempt, "oaat: connecting");
                match ConnectedEndpoint::connect(&config, endpoint_addr).await {
                    Ok(ep) => {
                        info!(device = %device_name, endpoint_name = %ep.info.endpoint_name, "oaat: connected");
                        endpoint = Some(ep);
                        break;
                    }
                    Err(e) => {
                        if attempt < 3 {
                            warn!(device = %device_name, error = %e, attempt, "oaat: connect failed, retry 1s");
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
                Err(_) => info!(device = %device_name, "oaat: clock sync timed out, continuing"),
            }

            let http_client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default();

            // Fetch & detect format
            let stream_id = format!("tune-{stream_num}");

<<<<<<< HEAD
            // If we have a local file path, read directly instead of HTTP self-fetch
            if let Some(ref fp) = file_path {
                debug!("reading file directly: {fp}");
                let file_data = match tokio::fs::read(fp).await {
                    Ok(d) => d,
                    Err(e) => {
                        debug!("file read failed: {e}");
                        error!(device = %device_name, error = %e, file = %fp, "oaat: file read failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let mut buf = file_data;
                let si = match detect_and_parse(&mut buf) {
                    Some(info) => info,
                    None => {
                        debug!("unsupported file format");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                debug!(
                    "OAAT-DEBUG: file format {:?} {}Hz {}bit {}ch, {} bytes PCM",
                    si.format,
                    si.sample_rate,
                    si.bits_per_sample,
                    si.channels,
                    buf.len()
                );

                let is_flac = si.format == AudioFormat::Flac;

                // For FLAC files, convert to WAV via ffmpeg then use WAV info
                let (pcm_data, cur_format, cur_sample_rate, cur_bits, ch) = if is_flac {
                    debug!("converting FLAC to WAV via ffmpeg...");
                    match super::helpers::decode_flac_to_pcm(fp) {
                        Some(wav_data) => {
                            let mut wav_buf = wav_data;
                            let wav_si = match detect_and_parse(&mut wav_buf) {
                                Some(info) => info,
                                None => {
                                    debug!("WAV parse failed after ffmpeg");
                                    playing.store(false, Ordering::SeqCst);
                                    return;
                                }
                            };
                            debug!(
                                "OAAT-DEBUG: WAV: {} bytes, {:?} {}Hz {}bit {}ch",
                                wav_buf.len(),
                                wav_si.format,
                                wav_si.sample_rate,
                                wav_si.bits_per_sample,
                                wav_si.channels
                            );
                            (
                                wav_buf,
                                wav_si.format,
                                wav_si.sample_rate,
                                wav_si.bits_per_sample,
                                wav_si.channels.min(8) as u8,
                            )
                        }
                        None => {
                            debug!("ffmpeg FLAC->WAV failed");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                } else {
                    (
                        buf,
                        si.format,
                        si.sample_rate,
                        si.bits_per_sample,
                        si.channels.min(8) as u8,
                    )
                };
                let layout = ChannelLayout::Stereo;
                let bytes_per_frame = (cur_bits as usize / 8) * si.channels as usize;

                let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
                if let Err(e) = endpoint
                    .propose_format(
                        &stream_id,
                        cur_format,
                        cur_sample_rate,
                        ch,
                        layout,
                        cur_bits as u8,
                    )
                    .await
                {
                    error!(device = %device_name, error = %e, "oaat: format propose failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }

                endpoint
                    .send_metadata(oaat_core::message::TrackMetadata {
                        title: title.clone(),
                        artist: artist.clone(),
                        album: album.clone(),
                        duration_ms: track_duration_ms,
                        artwork_url: cover_url.clone(),
                        format: Some(fmt_str),
                    })
                    .await
                    .ok();

                if let Err(e) = endpoint.send_play(&stream_id).await {
                    error!(device = %device_name, error = %e, "oaat: play failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }

                diag.connected.store(true, Ordering::SeqCst);
                debug!(
                    "OAAT-DEBUG: streaming {} bytes PCM directly",
                    pcm_data.len()
                );

                let packet_size = PCM_SAMPLES_PER_PACKET * bytes_per_frame;
                let mut offset = 0usize;
                let mut sample_offset: u64 = 0;
                let start = std::time::Instant::now();

                while offset < pcm_data.len() && playing.load(Ordering::Relaxed) {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    while paused.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if stop_rx.try_recv().is_ok() {
                            break;
                        }
                    }

                    let chunk_bytes = packet_size.min(pcm_data.len() - offset);
                    let chunk_samples = chunk_bytes / bytes_per_frame;
                    let payload = &pcm_data[offset..offset + chunk_bytes];
                    let pts_ns = (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64;
                    let flags = if offset == 0 {
                        PacketFlags::FIRST_PACKET
                    } else {
                        PacketFlags::empty()
                    };

                    if endpoint
                        .send_audio(
                            stream_num,
                            cur_format,
                            pts_ns,
                            sample_offset,
                            payload,
                            flags,
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }

                    offset += chunk_bytes;
                    sample_offset += chunk_samples as u64;
                    diag.packets_sent.fetch_add(1, Ordering::Relaxed);
                    diag.bytes_sent
                        .fetch_add(chunk_bytes as u64, Ordering::Relaxed);

                    position_ms.store(
                        sample_offset * 1000 / cur_sample_rate as u64,
                        Ordering::Relaxed,
                    );

                    let expected = std::time::Duration::from_nanos(
                        (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64,
                    );
                    let elapsed = start.elapsed();
                    if expected > elapsed {
                        tokio::time::sleep(expected - elapsed).await;
                    }
                }

                endpoint
                    .send_audio(
                        stream_num,
                        cur_format,
                        0,
                        sample_offset,
                        &[],
                        PacketFlags::LAST_PACKET,
                    )
                    .await
                    .ok();
                endpoint.send_stop(&stream_id).await.ok();
                playing.store(false, Ordering::SeqCst);
                diag.connected.store(false, Ordering::SeqCst);
                debug!(
                    "OAAT-DEBUG: direct file playback complete, {} samples",
                    sample_offset
                );
                return;
            }

            debug!(device = %device_name, url = %url, "oaat: fetching audio stream");
            let resp = match http_client.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    error!(device = %device_name, status = %r.status(), url = %url, "oaat: HTTP error");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    error!(device = %device_name, error = %e, url = %url, "oaat: fetch failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut stream: futures_util::stream::BoxStream<
                '_,
                Result<bytes::Bytes, reqwest::Error>,
            > = Box::pin(resp.bytes_stream());
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

            // Detect WAV or FLAC
            let si = match detect_and_parse(&mut buf) {
                Some(info) => info,
                None => {
                    let sig: Vec<u8> = buf.iter().take(12).copied().collect();
                    error!(device = %device_name, signature = %format!("{sig:02x?}"), "oaat: unsupported stream format");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let is_flac = si.format == AudioFormat::Flac;

            if is_flac {
                while buf.len() < 65536 {
                    match stream.next().await {
                        Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                        Some(Err(e)) => {
                            error!(device = %device_name, error = %e, "oaat: FLAC pre-buffer failed");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                        None => break,
                    }
                }
                debug!(device = %device_name, buffered = buf.len(), "oaat: FLAC pre-buffered");
            }

            let is_dsd = si.format.is_dsd();
            let uses_byte_offset = is_flac || is_dsd;
            let mut cur_format = si.format;
            let mut cur_sample_rate = si.sample_rate;
            let mut cur_bits = si.bits_per_sample;
            let cur_channels = si.channels;
            let ch = cur_channels.min(8) as u8;
            let layout = ChannelLayout::Stereo;

            let track_duration_ms = if track_duration_ms > 0 {
                track_duration_ms
            } else {
                si.duration_ms
            };
            duration_ms_arc.store(track_duration_ms, Ordering::SeqCst);

            let mut bytes_per_frame = (cur_bits as usize / 8) * cur_channels as usize;
            let mut packet_size = if is_dsd {
                DSD_CHUNK_SIZE
            } else if is_flac {
                FLAC_CHUNK_SIZE
            } else {
                PCM_SAMPLES_PER_PACKET * bytes_per_frame
            };

            info!(
                device = %device_name,
                sample_rate = cur_sample_rate, bits = cur_bits, channels = cur_channels,
                format = %cur_format, is_flac,
                "oaat: audio format detected"
            );

            // Format negotiation (use send_message directly to include dsd_rate)
            if let Err(e) = endpoint
                .send_message(&oaat_core::Message::FormatPropose(
                    oaat_core::message::FormatPropose {
                        stream_id: stream_id.clone(),
                        format: cur_format,
                        sample_rate: cur_sample_rate,
                        channels: ch,
                        channel_layout: layout,
                        bits_per_sample: cur_bits as u8,
                        dsd_rate: si.dsd_rate,
                    },
                ))
                .await
            {
                error!(device = %device_name, error = %e, "oaat: format propose failed");
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
                    info!(device = %device_name, rate = fc.sample_rate, bits = fc.bits_per_sample, "oaat: format counter-proposed");
                    cur_format = fc.format;
                    cur_bits = fc.bits_per_sample as u16;
                    cur_sample_rate = fc.sample_rate;
                    bytes_per_frame = (cur_bits as usize / 8) * cur_channels as usize;
                    packet_size = if cur_format == AudioFormat::Flac {
                        FLAC_CHUNK_SIZE
                    } else {
                        PCM_SAMPLES_PER_PACKET * bytes_per_frame
                    };
                }
                Ok(Some(oaat_controller::EndpointResponse::FormatReject(fr))) => {
                    error!(device = %device_name, reason = %fr.reason, "oaat: format rejected");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Ok(Some(other)) => {
                    warn!(device = %device_name, response = ?other, "oaat: unexpected response");
                }
                Ok(None) => {
                    error!(device = %device_name, "oaat: endpoint closed during negotiation");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(_) => {
                    error!(device = %device_name, "oaat: format negotiation timed out");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            }

            // Metadata + Play
            let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
            endpoint
                .send_metadata(oaat_core::message::TrackMetadata {
                    title,
                    artist,
                    album,
                    duration_ms: track_duration_ms,
                    artwork_url: cover_url,
                    format: Some(fmt_str),
                })
                .await
                .ok();

            if let Err(e) = endpoint.send_play(&stream_id).await {
                error!(device = %device_name, error = %e, "oaat: send_play failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }
            diag.connected.store(true, Ordering::SeqCst);
            diag.is_flac.store(is_flac, Ordering::SeqCst);
            diag.packets_sent.store(0, Ordering::SeqCst);
            diag.bytes_sent.store(0, Ordering::SeqCst);
            *diag.format_desc.lock().unwrap() =
                format_rate_display(cur_sample_rate, cur_bits, cur_format);
            info!(device = %device_name, "oaat: streaming started");

            // Build StreamInfo for reconnection
            let cur_stream_info = StreamInfo {
                sample_rate: cur_sample_rate,
                channels: cur_channels,
                bits_per_sample: cur_bits,
                format: cur_format,
                duration_ms: track_duration_ms,
                dsd_rate: dsd_rate_from_sample_rate(cur_sample_rate),
                data_offset: 0,
            };

            // Streaming loop
            let mut sample_offset: u64 = 0;
            let mut byte_offset: u64 = 0;
            let mut start = std::time::Instant::now();
            let mut pause_offset = std::time::Duration::ZERO;
            let mut reconnect_attempts: u32 = 0;

            let mut next_track: Option<NextTrackPrefetch> = None;
            let mut prefetch_rx: Option<tokio::sync::oneshot::Receiver<Option<NextTrackPrefetch>>> =
                None;

            let mut watchdog = tokio::time::interval(std::time::Duration::from_secs(10));
            watchdog.tick().await; // skip first immediate tick

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        debug!(device = %device_name, "oaat: stop signal");
                        break;
                    }

                    // Watchdog: detect stall (playing but no packets for 10s)
                    _ = watchdog.tick() => {
                        if playing.load(Ordering::Relaxed) && !paused.load(Ordering::Relaxed) {
                            let last = diag.last_packet_epoch_ms.load(Ordering::Relaxed);
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            if last > 0 && now.saturating_sub(last) > 10_000 {
                                warn!(device = %device_name, stale_ms = now - last, "oaat: watchdog — stall detected, attempting reconnect");
                                diag.reconnects.fetch_add(1, Ordering::Relaxed);
                                match connect_and_setup(&config, endpoint_addr, &device_name, &stream_id, &cur_stream_info).await {
                                    Some(new_ep) => {
                                        endpoint = new_ep;
                                        diag.connected.store(true, Ordering::SeqCst);
                                        info!(device = %device_name, "oaat: watchdog reconnected");
                                    }
                                    None => {
                                        warn!(device = %device_name, "oaat: watchdog reconnect failed");
                                    }
                                }
                            }
                        }
                    }

                    result = async {
                        match prefetch_rx.as_mut() {
                            Some(rx) => rx.await.ok(),
                            None => std::future::pending().await,
                        }
                    } => {
                        prefetch_rx = None;
                        if let Some(Some(mut prefetch)) = result {
                            if prefetch.same_format {
                                info!(device = %device_name, title = %prefetch.title, "oaat: next track prefetched (gapless ready)");
                                if let Ok(()) = endpoint.prepare_next_track(
                                    &stream_id, cur_format, cur_sample_rate, ch, layout, cur_bits as u8,
                                ).await {
                                    match tokio::time::timeout(std::time::Duration::from_secs(2), endpoint.response_rx.recv()).await {
                                        Ok(Some(oaat_controller::EndpointResponse::NextTrackReady(_))) => {
                                            info!(device = %device_name, "oaat: gapless confirmed");
                                        }
                                        Ok(Some(oaat_controller::EndpointResponse::NextTrackReformat(_))) => {
                                            prefetch.same_format = false;
                                        }
                                        _ => {}
                                    }
                                }
                            } else {
                                info!(device = %device_name, title = %prefetch.title, "oaat: next track prefetched (format change)");
                            }
                            next_track = Some(prefetch);
                        }
                    }

                    Some(cmd) = command_rx.recv() => {
                        match cmd {
                            OaatCommand::Pause => {
                                paused.store(true, Ordering::SeqCst);
                                pause_offset = start.elapsed();
                                endpoint.send_message(&oaat_core::Message::Pause(oaat_core::message::Pause {
                                    stream_id: stream_id.clone(),
                                })).await.ok();
                                info!(device = %device_name, "oaat: paused");
                            }
                            OaatCommand::Resume => {
                                paused.store(false, Ordering::SeqCst);
                                start = std::time::Instant::now() - pause_offset;
                                endpoint.send_play(&stream_id).await.ok();
                                info!(device = %device_name, "oaat: resumed");
                            }
                            OaatCommand::SetVolume(level) => { endpoint.send_volume(level).await.ok(); }
                            OaatCommand::Mute(muted) => { endpoint.send_mute(muted).await.ok(); }
                            OaatCommand::Seek { position_ms: seek_pos } => {
                                // Tell endpoint about seek
                                endpoint.send_message(&oaat_core::Message::Seek(oaat_core::message::Seek {
                                    stream_id: stream_id.clone(),
                                    position_ms: seek_pos,
                                })).await.ok();

                                if is_flac {
                                    match http_client.get(&url).send().await {
                                        Ok(resp) if resp.status().is_success() => {
                                            stream = Box::pin(resp.bytes_stream());
                                            buf.clear();
                                            let mut header_ok = true;
                                            while buf.len() < 65536 {
                                                match stream.next().await {
                                                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                                                    Some(Err(_)) | None => { header_ok = false; break; }
                                                }
                                            }
                                            if header_ok {
                                                byte_offset = 0;
                                                sample_offset = 0;
                                                let elapsed_eq = std::time::Duration::from_millis(seek_pos);
                                                start = std::time::Instant::now() - elapsed_eq;
                                                pause_offset = std::time::Duration::ZERO;
                                                position_ms.store(seek_pos, Ordering::SeqCst);
                                                info!(device = %device_name, seek_pos, "oaat: FLAC seek — stream restarted");
                                            } else {
                                                warn!(device = %device_name, "oaat: FLAC seek re-buffer failed");
                                            }
                                        }
                                        Ok(resp) => warn!(device = %device_name, status = %resp.status(), "oaat: FLAC seek re-fetch failed"),
                                        Err(e) => warn!(device = %device_name, error = %e, "oaat: FLAC seek re-fetch failed"),
                                    }
                                } else {
                                    // Calculate byte offset
                                    let bytes_per_sec = if is_dsd {
                                        cur_sample_rate as u64 * cur_channels as u64 / 8
                                    } else {
                                        cur_sample_rate as u64 * bytes_per_frame as u64
                                    };
                                    let data_byte = seek_pos * bytes_per_sec / 1000;
                                    let frame_align = if is_dsd { 4096 * cur_channels as u64 } else { bytes_per_frame as u64 };
                                    let aligned = (data_byte / frame_align) * frame_align;
                                    let file_offset = aligned + cur_stream_info.data_offset as u64;

                                    let range = format!("bytes={file_offset}-");
                                    match http_client.get(&url).header("Range", &range).send().await {
                                        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 206 => {
                                            stream = Box::pin(resp.bytes_stream());
                                            buf.clear();
                                            if uses_byte_offset { byte_offset = aligned; } else { sample_offset = aligned / bytes_per_frame as u64; }
                                            let elapsed_eq = std::time::Duration::from_millis(seek_pos);
                                            start = std::time::Instant::now() - elapsed_eq;
                                            pause_offset = std::time::Duration::ZERO;
                                            position_ms.store(seek_pos, Ordering::SeqCst);
                                            info!(device = %device_name, seek_pos, file_offset, "oaat: seek complete");
                                        }
                                        Ok(resp) => warn!(device = %device_name, status = %resp.status(), "oaat: seek Range failed"),
                                        Err(e) => warn!(device = %device_name, error = %e, "oaat: seek request failed"),
                                    }
                                }
                            }
                            OaatCommand::PrepareNext { url, title, artist, album, cover_url, duration_ms } => {
                                let client = http_client.clone();
                                let dev = device_name.clone();
                                let cur_fmt = cur_format;
                                let cur_rate = cur_sample_rate;
                                let cur_bps = cur_bits;
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                prefetch_rx = Some(rx);
                                tokio::spawn(async move {
                                    let _ = tx.send(prefetch_next_track(&client, &dev, &url, title, artist, album, cover_url, duration_ms, cur_fmt, cur_rate, cur_bps).await);
                                });
                            }
                        }
                    }

                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(data)) => buf.extend_from_slice(&data),
                            Some(Err(e)) => {
                                error!(device = %device_name, error = %e, "oaat: stream error");
                                break;
                            }
                            None => {
                                // Flush remaining buffer
                                while buf.len() >= packet_size && playing.load(Ordering::Relaxed) {
                                    let payload: Vec<u8> = buf.drain(..packet_size).collect();
                                    let pts_ns = if uses_byte_offset {
                                        (byte_offset as f64 / (cur_sample_rate as f64 * bytes_per_frame as f64) * 1e9) as u64
                                    } else {
                                        (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64
                                    };
                                    let _ = endpoint.send_audio(stream_num, cur_format, pts_ns, sample_offset, &payload, PacketFlags::empty()).await;
                                    if uses_byte_offset { byte_offset += payload.len() as u64; }
                                    else { sample_offset += PCM_SAMPLES_PER_PACKET as u64; }
                                    position_ms.store(
                                        if uses_byte_offset { byte_offset * 1000 / (cur_sample_rate as u64 * bytes_per_frame as u64).max(1) }
                                        else { sample_offset * 1000 / cur_sample_rate as u64 },
                                        Ordering::Relaxed,
                                    );
                                }

                                // Gapless transition
                                if let Some(next) = next_track.take() {
                                    info!(device = %device_name, title = %next.title, "oaat: gapless transition");

                                    if !next.same_format {
                                        if let Err(e) = endpoint.propose_format(&stream_id, next.info.format, next.info.sample_rate, ch, layout, next.info.bits_per_sample as u8).await {
                                            error!(device = %device_name, error = %e, "oaat: re-negotiate failed");
                                            break;
                                        }
                                        match tokio::time::timeout(std::time::Duration::from_secs(5), endpoint.response_rx.recv()).await {
                                            Ok(Some(oaat_controller::EndpointResponse::FormatAccept(_))) |
                                            Ok(Some(oaat_controller::EndpointResponse::FormatCounter(_))) => {
                                                cur_format = next.info.format;
                                                cur_bits = next.info.bits_per_sample;
                                                cur_sample_rate = next.info.sample_rate;
                                                bytes_per_frame = (cur_bits as usize / 8) * cur_channels as usize;
                                                packet_size = if cur_format == AudioFormat::Flac { FLAC_CHUNK_SIZE } else { PCM_SAMPLES_PER_PACKET * bytes_per_frame };
                                            }
                                            _ => { error!(device = %device_name, "oaat: re-negotiate failed for next track"); break; }
                                        }
                                    }

                                    *current_title.lock().await = Some(next.title.clone());
                                    *current_artist.lock().await = Some(next.artist.clone());
                                    *current_uri.lock().await = Some(String::new());
                                    duration_ms_arc.store(next.duration_ms, Ordering::SeqCst);

                                    let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
                                    endpoint.send_metadata(oaat_core::message::TrackMetadata {
                                        title: next.title, artist: next.artist, album: next.album,
                                        duration_ms: next.duration_ms, artwork_url: next.cover_url,
                                        format: Some(fmt_str),
                                    }).await.ok();

                                    sample_offset = 0;
                                    byte_offset = 0;
                                    position_ms.store(0, Ordering::SeqCst);
                                    start = std::time::Instant::now();
                                    buf = next.buf;
                                    stream = next.stream;
                                    continue;
                                }
                                break;
                            }
                        }

                        // Send buffered packets
                        while buf.len() >= packet_size
                            && playing.load(Ordering::Relaxed)
                            && !paused.load(Ordering::Relaxed)
                        {
                            let payload: Vec<u8> = buf.drain(..packet_size).collect();
                            let pts_ns = if uses_byte_offset {
                                (byte_offset as f64 / (cur_sample_rate as f64 * bytes_per_frame as f64) * 1e9) as u64
                            } else {
                                (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64
                            };
                            let flags = if sample_offset == 0 && byte_offset == 0 {
                                PacketFlags::FIRST_PACKET
                            } else {
                                PacketFlags::empty()
                            };

                            match endpoint.send_audio(stream_num, cur_format, pts_ns, sample_offset, &payload, flags).await {
                                Ok(()) => {
                                    reconnect_attempts = 0;
                                    diag.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    diag.bytes_sent.fetch_add(payload.len() as u64, Ordering::Relaxed);
                                    diag.last_packet_epoch_ms.store(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                                Err(_) => {
                                    // Reconnection mid-stream
                                    if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                                        error!(device = %device_name, "oaat: send_audio failed, max reconnects reached");
                                        break;
                                    }
                                    reconnect_attempts += 1;
                                    diag.reconnects.fetch_add(1, Ordering::Relaxed);
                                    warn!(device = %device_name, attempt = reconnect_attempts, "oaat: send_audio failed, reconnecting");

                                    // Put payload back
                                    let mut restored = payload;
                                    restored.extend_from_slice(&buf);
                                    buf = restored;

                                    match connect_and_setup(&config, endpoint_addr, &device_name, &stream_id, &cur_stream_info).await {
                                        Some(new_ep) => {
                                            endpoint = new_ep;
                                            info!(device = %device_name, "oaat: reconnected, resuming stream");
                                            continue;
                                        }
                                        None => {
                                            error!(device = %device_name, "oaat: reconnection failed");
                                            break;
                                        }
                                    }
                                }
                            }

                            if sample_offset == 0 && byte_offset == 0 {
                                info!(device = %device_name, payload_bytes = payload.len(), "oaat: first audio packet sent");
                            }

                            if uses_byte_offset {
                                byte_offset += payload.len() as u64;
                            } else {
                                sample_offset += PCM_SAMPLES_PER_PACKET as u64;
                            }
                            position_ms.store(
                                if uses_byte_offset { byte_offset * 1000 / (cur_sample_rate as u64 * bytes_per_frame as u64).max(1) }
                                else { sample_offset * 1000 / cur_sample_rate as u64 },
                                Ordering::Relaxed,
                            );

                            // Real-time pacing
                            let expected = if uses_byte_offset {
                                let audio_bytes_per_sec = cur_sample_rate as f64 * bytes_per_frame as f64;
                                std::time::Duration::from_nanos(
                                    (byte_offset as f64 / audio_bytes_per_sec * 1e9) as u64,
                                )
                            } else {
                                std::time::Duration::from_nanos(
                                    (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64,
                                )
                            };
                            let elapsed = start.elapsed();
                            if expected > elapsed {
                                tokio::time::sleep(expected - elapsed).await;
                            }
                        }
                    }
                }
            }

            endpoint.send_stop(&stream_id).await.ok();
            playing.store(false, Ordering::SeqCst);
            diag.connected.store(false, Ordering::SeqCst);
            let duration_s = start.elapsed().as_secs_f64();
            let packets = if uses_byte_offset {
                byte_offset / FLAC_CHUNK_SIZE as u64
            } else {
                sample_offset / PCM_SAMPLES_PER_PACKET as u64
            };
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

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Seek { position_ms }).await;
        }
        info!(device = %self.name, position_ms, "oaat: seek");
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume.clamp(0.0, 1.0) * 255.0) as u8;
        self.volume.store(level as u32, Ordering::SeqCst);
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

    #[cfg(feature = "oaat")]
    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            tx.send(OaatCommand::PrepareNext {
                url: url.to_owned(),
                title: title.unwrap_or("Unknown").to_owned(),
                artist: artist.unwrap_or("Unknown").to_owned(),
                album: String::new(),
                cover_url: None,
                duration_ms: 0,
            })
            .await
            .map_err(|e| format!("channel closed: {e}"))?;
            info!(device = %self.name, title = ?title, "oaat: next track queued");
        }
        Ok(())
    }

    #[cfg(feature = "oaat")]
    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            tx.send(OaatCommand::PrepareNext {
                url: media.url.to_owned(),
                title: media.title.unwrap_or("Unknown").to_owned(),
                artist: media.artist.unwrap_or("Unknown").to_owned(),
                album: media.album.unwrap_or("").to_owned(),
                cover_url: media.cover_url.map(|s| s.to_owned()),
                duration_ms: media.duration_ms.unwrap_or(0),
            })
            .await
            .map_err(|e| format!("channel closed: {e}"))?;
            info!(device = %self.name, title = ?media.title, "oaat: next track queued");
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
        true
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        Some(self.diagnostics_snapshot())
    }
}

#[cfg(feature = "oaat")]
async fn prefetch_next_track(
    client: &reqwest::Client,
    device_name: &str,
    url: &str,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
    cur_format: oaat_core::format::AudioFormat,
    cur_sample_rate: u32,
    cur_bits: u16,
) -> Option<NextTrackPrefetch> {
    use futures_util::StreamExt;

    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            error!(device = %device_name, status = %r.status(), "oaat: next track HTTP error");
            return None;
        }
        Err(e) => {
            error!(device = %device_name, error = %e, "oaat: next track fetch failed");
            return None;
        }
    };

    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();

    while buf.len() < 128 {
        match stream.next().await {
            Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
            _ => {
                error!(device = %device_name, "oaat: next track stream ended before header");
                return None;
            }
        }
    }

    let si = match detect_and_parse(&mut buf) {
        Some(info) => info,
        None => {
            error!(device = %device_name, "oaat: next track format unsupported");
            return None;
        }
    };

    let duration_ms = if duration_ms > 0 {
        duration_ms
    } else {
        si.duration_ms
    };
    let same_format = si.format == cur_format
        && si.sample_rate == cur_sample_rate
        && si.bits_per_sample == cur_bits;

    info!(
        device = %device_name, title = %title,
        format = %si.format, sample_rate = si.sample_rate, bits = si.bits_per_sample,
        same_format, "oaat: next track prefetched"
    );

    Some(NextTrackPrefetch {
        stream: stream.boxed(),
        buf,
        info: si,
        title,
        artist,
        album,
        cover_url,
        duration_ms,
        same_format,
    })
}
