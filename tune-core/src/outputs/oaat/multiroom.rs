use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::Mutex;
use tracing::info;
#[cfg(feature = "oaat")]
use tracing::{error, warn};

use crate::outputs::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

#[cfg(feature = "oaat")]
use super::helpers::{detect_and_parse, format_rate_display};

#[cfg(feature = "oaat")]
const FLAC_CHUNK_SIZE: usize = 4096;
#[cfg(feature = "oaat")]
const PCM_SAMPLES_PER_PACKET: usize = 480;

#[cfg(feature = "oaat")]
type SharedZone = Arc<Mutex<oaat_controller::Zone>>;

pub struct OaatMultiroomOutput {
    name: String,
    device_id: String,
    endpoints: Vec<(String, u16)>,
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
    zone: SharedZone,
    #[cfg(feature = "oaat")]
    zone_config: oaat_controller::ControllerConfig,
}

impl OaatMultiroomOutput {
    pub fn new(name: String, group_id: String, endpoints: Vec<(String, u16)>) -> Self {
        let device_id = format!("oaat-group:{group_id}");
        let controller_id = uuid::Uuid::new_v4().to_string();

        #[cfg(feature = "oaat")]
        let zone_config = oaat_controller::ControllerConfig {
            controller_id: controller_id.clone(),
            controller_name: "Tune Server".into(),
            features: vec![],
            clock_port: oaat_core::DEFAULT_CLOCK_PORT,
            tls: false,
        };

        #[cfg(feature = "oaat")]
        let zone = Arc::new(Mutex::new(oaat_controller::Zone::new(
            group_id.clone(),
            name.clone(),
            zone_config.clone(),
        )));

        Self {
            name,
            device_id,
            endpoints,
            controller_id,
            stream_counter: Arc::new(AtomicU32::new(1)),
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(200)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(Mutex::new(None)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            stop_tx: Mutex::new(None),
            #[cfg(feature = "oaat")]
            zone,
            #[cfg(feature = "oaat")]
            zone_config,
        }
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Add an endpoint dynamically (late-join if streaming).
    #[cfg(feature = "oaat")]
    pub async fn add_endpoint(&self, host: &str, port: u16) -> Result<String, String> {
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| format!("invalid address: {e}"))?;

        let mut zone = self.zone.lock().await;
        let ep_id = if zone.is_streaming() {
            zone.join_active(addr)
                .await
                .map_err(|e| format!("late-join failed: {e}"))?
        } else {
            zone.add_endpoint(addr)
                .await
                .map_err(|e| format!("add failed: {e}"))?
        };

        info!(
            device = %self.name,
            endpoint_id = %ep_id,
            addr = %addr,
            streaming = zone.is_streaming(),
            "oaat-multiroom: endpoint added dynamically"
        );
        Ok(ep_id)
    }

    /// Remove an endpoint dynamically.
    #[cfg(feature = "oaat")]
    pub async fn remove_endpoint(&self, endpoint_id: &str) -> bool {
        let mut zone = self.zone.lock().await;
        let removed = zone.remove_endpoint_and_notify(endpoint_id).await;
        if removed {
            info!(device = %self.name, endpoint_id, "oaat-multiroom: endpoint removed dynamically");
        }
        removed
    }

    /// Set zone master volume (all endpoints).
    #[cfg(feature = "oaat")]
    pub async fn set_zone_volume(&self, level: u8) -> Result<(), String> {
        let mut zone = self.zone.lock().await;
        zone.set_volume_all(level)
            .await
            .map_err(|e| format!("volume failed: {e}"))
    }

    /// Set per-endpoint volume (absolute level).
    #[cfg(feature = "oaat")]
    pub async fn set_endpoint_volume(&self, endpoint_id: &str, level: u8) -> Result<(), String> {
        let mut zone = self.zone.lock().await;
        zone.set_volume_endpoint(endpoint_id, level)
            .await
            .map_err(|e| format!("volume failed: {e}"))
    }

    /// Set per-endpoint volume offset (relative to master).
    #[cfg(feature = "oaat")]
    pub async fn set_endpoint_volume_offset(
        &self,
        endpoint_id: &str,
        offset: i8,
    ) -> Result<(), String> {
        let mut zone = self.zone.lock().await;
        zone.set_volume_offset(endpoint_id, offset)
            .await
            .map_err(|e| format!("volume offset failed: {e}"))
    }

    /// Get zone status snapshot.
    #[cfg(feature = "oaat")]
    pub async fn zone_snapshot(&self) -> serde_json::Value {
        let zone = self.zone.lock().await;
        let snaps = zone.endpoint_snapshots();
        let vol = zone.volume_map();
        serde_json::json!({
            "zone_id": zone.zone_id,
            "name": zone.name,
            "streaming": zone.is_streaming(),
            "multiroom": zone.is_multiroom(),
            "master_volume": vol.master,
            "endpoints": snaps.iter().map(|s| serde_json::json!({
                "endpoint_id": s.endpoint_id,
                "name": s.endpoint_name,
                "addr": s.addr.to_string(),
                "state": s.state.to_string(),
                "volume_offset": s.volume_offset,
                "effective_volume": vol.effective_volume(&s.endpoint_id),
            })).collect::<Vec<_>>(),
        })
    }

    /// Run health check and return dead endpoint IDs.
    #[cfg(feature = "oaat")]
    pub async fn check_health(&self) -> Vec<String> {
        let zone = self.zone.lock().await;
        zone.check_health()
    }

    /// Prune disconnected endpoints.
    #[cfg(feature = "oaat")]
    pub async fn prune_dead(&self) -> Vec<String> {
        let mut zone = self.zone.lock().await;
        let dead = zone.check_health();
        for id in &dead {
            zone.mark_disconnected(id);
        }
        zone.prune_disconnected()
    }
}

#[async_trait::async_trait]
impl OutputTarget for OaatMultiroomOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "oaat-multiroom"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(feature = "oaat")]
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        use oaat_core::ChannelLayout;
        use oaat_core::format::AudioFormat;
        use oaat_core::wire::PacketFlags;

        self.stop().await.ok();

        let url = media.url.to_owned();
        let title = media.title.unwrap_or("Unknown").to_owned();
        let artist = media.artist.unwrap_or("Unknown").to_owned();
        let album = media.album.unwrap_or("").to_owned();
        let cover_url = media.cover_url.map(|s| s.to_owned());
        let track_duration_ms = media.duration_ms.unwrap_or(0);

        *self.current_uri.lock().await = Some(url.clone());
        *self.current_title.lock().await = Some(title.clone());
        *self.current_artist.lock().await = Some(artist.clone());
        self.duration_ms.store(track_duration_ms, Ordering::SeqCst);

        let endpoint_addrs: Vec<SocketAddr> = self
            .endpoints
            .iter()
            .filter_map(|(host, port)| format!("{host}:{port}").parse().ok())
            .collect();

        info!(
            device = %self.name,
            endpoints = endpoint_addrs.len(),
            title = %title,
            "oaat-multiroom: play_media"
        );

        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let position_ms = self.position_ms.clone();
        let duration_ms_arc = self.duration_ms.clone();
        let device_name = self.name.clone();
        let stream_num = self.stream_counter.fetch_add(1, Ordering::SeqCst);
        let zone = self.zone.clone();

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tx.lock().await = Some(stop_tx);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(0, Ordering::SeqCst);

        tokio::spawn(async move {
            use futures_util::StreamExt;

            // Connect initial endpoints to zone
            let mut connected = 0usize;
            {
                let mut z = zone.lock().await;
                for addr in &endpoint_addrs {
                    match z.add_endpoint(*addr).await {
                        Ok(eid) => {
                            info!(device = %device_name, endpoint_id = %eid, addr = %addr, "oaat-multiroom: endpoint added");
                            connected += 1;
                        }
                        Err(e) => {
                            warn!(device = %device_name, addr = %addr, error = %e, "oaat-multiroom: endpoint connect failed, skipping");
                        }
                    }
                }
            }

            if connected == 0 {
                error!(device = %device_name, "oaat-multiroom: no endpoints connected");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            info!(device = %device_name, connected, total = endpoint_addrs.len(), "oaat-multiroom: zone ready");

            {
                zone.lock().await.start_steady_clock_sync();
            }

            // Fetch audio stream
            let http_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default();

            let stream_id = format!("tune-{stream_num}");

            let resp = match http_client.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    error!(device = %device_name, status = %r.status(), "oaat-multiroom: HTTP error");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    error!(device = %device_name, error = %e, "oaat-multiroom: fetch failed");
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
                        error!(device = %device_name, "oaat-multiroom: stream ended before header");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }

            let si = match detect_and_parse(&mut buf) {
                Some(info) => info,
                None => {
                    error!(device = %device_name, "oaat-multiroom: unsupported stream format");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let is_flac = si.format == AudioFormat::Flac;
            let cur_format = si.format;
            let cur_sample_rate = si.sample_rate;
            let cur_bits = si.bits_per_sample;
            let ch = si.channels.min(8) as u8;
            let layout = ChannelLayout::Stereo;
            let bytes_per_frame = (cur_bits as usize / 8) * si.channels as usize;
            let packet_size = if is_flac {
                FLAC_CHUNK_SIZE
            } else {
                PCM_SAMPLES_PER_PACKET * bytes_per_frame
            };

            let track_duration_ms = if track_duration_ms > 0 {
                track_duration_ms
            } else {
                si.duration_ms
            };
            duration_ms_arc.store(track_duration_ms, Ordering::SeqCst);

            info!(
                device = %device_name,
                format = %cur_format, sample_rate = cur_sample_rate, bits = cur_bits,
                "oaat-multiroom: format detected"
            );

            {
                let mut z = zone.lock().await;
                if let Err(e) = z
                    .propose_format_all(
                        &stream_id,
                        cur_format,
                        cur_sample_rate,
                        ch,
                        layout,
                        cur_bits as u8,
                    )
                    .await
                {
                    error!(device = %device_name, error = %e, "oaat-multiroom: format negotiation failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }

                let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
                z.send_metadata_all(oaat_core::message::TrackMetadata {
                    title,
                    artist,
                    album,
                    duration_ms: track_duration_ms,
                    artwork_url: cover_url,
                    format: Some(fmt_str),
                })
                .await
                .ok();

                if let Err(e) = z.play_all(&stream_id).await {
                    error!(device = %device_name, error = %e, "oaat-multiroom: play failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            }

            info!(device = %device_name, endpoints = connected, "oaat-multiroom: synchronized streaming started");

            // Streaming loop — fan-out via Zone
            let mut sample_offset: u64 = 0;
            let mut byte_offset: u64 = 0;
            let start = std::time::Instant::now();
            let mut health_check_interval =
                tokio::time::interval(std::time::Duration::from_secs(10));
            health_check_interval.tick().await; // skip immediate first tick

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        info!(device = %device_name, "oaat-multiroom: stop signal");
                        break;
                    }
                    _ = health_check_interval.tick() => {
                        let mut z = zone.lock().await;
                        let dead = z.check_health();
                        for id in &dead {
                            z.mark_disconnected(id);
                            warn!(device = %device_name, endpoint_id = %id, "oaat-multiroom: endpoint died, marked disconnected");
                        }
                        z.prune_disconnected();
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(data)) => buf.extend_from_slice(&data),
                            Some(Err(e)) => {
                                error!(device = %device_name, error = %e, "oaat-multiroom: stream error");
                                break;
                            }
                            None => break,
                        }

                        while buf.len() >= packet_size
                            && playing.load(Ordering::Relaxed)
                            && !paused.load(Ordering::Relaxed)
                        {
                            let payload: Vec<u8> = buf.drain(..packet_size).collect();
                            let pts_ns = if is_flac {
                                (byte_offset as f64 / (cur_sample_rate as f64 * bytes_per_frame as f64) * 1e9) as u64
                            } else {
                                (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64
                            };
                            let flags = if sample_offset == 0 && byte_offset == 0 {
                                PacketFlags::FIRST_PACKET
                            } else {
                                PacketFlags::empty()
                            };

                            {
                                let mut z = zone.lock().await;
                                if z.send_audio_all(stream_num, cur_format, pts_ns, sample_offset, &payload, flags).await.is_err() {
                                    error!(device = %device_name, "oaat-multiroom: send_audio_all failed");
                                    break;
                                }
                            }

                            if sample_offset == 0 && byte_offset == 0 {
                                info!(device = %device_name, endpoints = connected, "oaat-multiroom: first packet sent to all endpoints");
                            }

                            if is_flac { byte_offset += payload.len() as u64; }
                            else { sample_offset += PCM_SAMPLES_PER_PACKET as u64; }

                            position_ms.store(
                                if is_flac { byte_offset * 1000 / (cur_sample_rate as u64 * bytes_per_frame as u64).max(1) }
                                else { sample_offset * 1000 / cur_sample_rate as u64 },
                                Ordering::Relaxed,
                            );

                            let expected = if is_flac {
                                let audio_bps = cur_sample_rate as f64 * bytes_per_frame as f64;
                                std::time::Duration::from_nanos((byte_offset as f64 / audio_bps * 1e9) as u64)
                            } else {
                                std::time::Duration::from_nanos((sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64)
                            };
                            let elapsed = start.elapsed();
                            if expected > elapsed {
                                tokio::time::sleep(expected - elapsed).await;
                            }
                        }
                    }
                }
            }

            {
                let mut z = zone.lock().await;
                z.stop_all(&stream_id).await.ok();
            }
            playing.store(false, Ordering::SeqCst);
            let duration_s = start.elapsed().as_secs_f64();
            let packets = if is_flac {
                byte_offset / FLAC_CHUNK_SIZE as u64
            } else {
                sample_offset / PCM_SAMPLES_PER_PACKET as u64
            };
            info!(device = %device_name, samples = sample_offset, packets, duration_s = format!("{duration_s:.1}"), "oaat-multiroom: complete");
        });

        Ok(())
    }

    #[cfg(not(feature = "oaat"))]
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("OAAT support not compiled (enable 'oaat' feature)".into())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        info!(device = %self.name, "oaat-multiroom: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "oaat-multiroom: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        *self.current_uri.lock().await = None;
        info!(device = %self.name, "oaat-multiroom: stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume.clamp(0.0, 1.0) * 100.0) as u8;
        self.volume.store(level as u32, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        {
            let mut zone = self.zone.lock().await;
            zone.set_volume_all(level).await.ok();
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        #[cfg(feature = "oaat")]
        {
            let mut zone = self.zone.lock().await;
            zone.set_mute_all(muted).await.ok();
        }
        if muted {
            self.volume.store(0, Ordering::SeqCst);
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
            volume: self.volume.load(Ordering::Relaxed) as f64 / 100.0,
            muted: self.volume.load(Ordering::Relaxed) == 0,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
        })
    }

    async fn is_available(&self) -> bool {
        true
    }

    #[cfg(feature = "oaat")]
    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        None
    }
}
