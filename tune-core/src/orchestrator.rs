use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::audio::formats::AudioFormat;
use crate::db::history_repo::{HistoryRepo, ListenRecord};
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::prefetch::PrefetchEngine;
use crate::streaming::registry::ServiceRegistry;

pub struct PlaybackOrchestrator {
    pub db: Arc<dyn crate::db::backend::DbBackend>,
    pub playback: Arc<PlaybackManager>,
    pub streamer: Arc<AudioStreamer>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub advertised_ip: Option<String>,
    pub event_bus: Option<Arc<EventBus>>,
    gapless_sessions: Mutex<HashMap<i64, String>>,
    pub prefetch: Arc<PrefetchEngine>,
    dsd_capabilities: Mutex<HashMap<String, crate::outputs::dlna::DsdCapability>>,
    /// Cache of MIME types that each DLNA renderer does NOT support.
    /// Key: device_id, Value: set of unsupported MIME types (e.g. "audio/flac").
    /// Only negative results are cached — if a MIME is not in the set, it's
    /// either supported or hasn't been checked yet.
    dlna_unsupported_mimes: Mutex<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Default)]
pub struct PlayRequest {
    pub zone_id: i64,
    pub output_device_id: Option<String>,
    pub track_id: Option<i64>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_url: Option<String>,
    pub duration_ms: Option<i64>,
    pub seek_ms: Option<u64>,
    pub temp_file_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlayResult {
    pub stream_url: Option<String>,
    pub output_sent: bool,
    pub source: String,
    pub error: Option<String>,
}

pub struct ResolvedStream {
    pub url: String,
    pub mime_type: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    pub source: String,
    pub cover_url: Option<String>,
    pub stream_id: Option<String>,
    pub file_size: Option<u64>,
    /// Audio sample rate in Hz for the output stream (e.g. 176400 for DSD64->PCM).
    pub sample_rate: Option<u32>,
    /// Output bit depth (e.g. 24 for DSD->PCM).
    pub bit_depth: Option<u32>,
    /// Number of audio channels.
    pub channels: Option<u32>,
}

pub struct ResolvedQueueItem {
    pub url: String,
    pub mime_type: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub duration_ms: Option<u64>,
    pub stream_id: Option<String>,
    /// Audio sample rate in Hz (e.g. 44100, 96000).
    pub sample_rate: Option<u32>,
    /// Audio bit depth (e.g. 16, 24).
    pub bit_depth: Option<u32>,
    /// Number of audio channels (e.g. 2 for stereo).
    pub channels: Option<u32>,
    /// File size in bytes for the stream.
    pub file_size: Option<u64>,
}

impl PlaybackOrchestrator {
    pub fn new(
        db: Arc<dyn crate::db::backend::DbBackend>,
        playback: Arc<PlaybackManager>,
        streamer: Arc<AudioStreamer>,
        services: Arc<Mutex<ServiceRegistry>>,
        outputs: Arc<Mutex<OutputRegistry>>,
        advertised_ip: Option<String>,
    ) -> Self {
        Self {
            db,
            playback,
            streamer,
            services,
            outputs,
            advertised_ip,
            event_bus: None,
            gapless_sessions: Mutex::new(HashMap::new()),
            prefetch: Arc::new(PrefetchEngine::new()),
            dsd_capabilities: Mutex::new(HashMap::new()),
            dlna_unsupported_mimes: Mutex::new(HashMap::new()),
        }
    }

    /// Remove any gapless-prepared stream session for a zone.
    /// Called when a zone starts a new track or stops, so the
    /// previously prepared session doesn't leak.
    async fn cleanup_gapless_session(&self, zone_id: i64) {
        let old_sid = self.gapless_sessions.lock().await.remove(&zone_id);
        if let Some(ref sid) = old_sid {
            self.streamer.remove_session(sid).await;
            debug!(zone_id, stream_id = %sid, "gapless_session_cleaned_up");
        }
    }

    fn server_ip(&self) -> String {
        self.advertised_ip.clone().unwrap_or_else(|| {
            crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into())
        })
    }

    pub async fn play(&self, mut req: PlayRequest) -> Result<PlayResult, String> {
        let play_start = std::time::Instant::now();
        // Ensure output_device_id is populated: if the caller didn't provide
        // it (e.g. web client sends only zone_id + track_id), look it up from
        // the zone's DB record.  This is the primary gate for send_to_output —
        // without it, the stream is created but never sent to the output device.
        if req.output_device_id.is_none() {
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();

            // Refuse to start playback on a zone whose device is confirmed gone.
            // Guards: skip local: zones (always available), skip zones with no
            // device yet (being configured), and allow a grace window for SSDP
            // polling gaps by checking the live OutputRegistry — if the device is
            // still registered it is reachable even if the DB says offline.
            if let Some(ref zone) = zone_db {
                if !zone.online {
                    let dev_id = zone.output_device_id.as_deref().unwrap_or("");
                    let is_local = dev_id.starts_with("local:");
                    let has_device = !dev_id.is_empty();
                    let in_registry =
                        has_device && !is_local && self.outputs.lock().await.contains(dev_id);
                    if has_device && !is_local && !in_registry {
                        let msg = format!("Output device offline: {}", zone.name);
                        warn!(zone_id = req.zone_id, zone_name = %zone.name, "play_rejected_zone_offline");
                        if let Some(ref bus) = self.event_bus {
                            bus.emit(
                                "zone.playback_error",
                                serde_json::json!({
                                    "zone_id": req.zone_id,
                                    "error": msg,
                                }),
                            );
                        }
                        return Err(msg);
                    }
                }
            }

            let looked_up = zone_db.and_then(|z| z.output_device_id);
            if looked_up.is_some() {
                debug!(
                    zone_id = req.zone_id,
                    device_id = ?looked_up,
                    "output_device_id_resolved_from_zone_db"
                );
            } else {
                warn!(
                    zone_id = req.zone_id,
                    "output_device_id_missing_not_in_request_nor_zone_db"
                );
            }
            req.output_device_id = looked_up;
        } else {
            // output_device_id was provided by the caller — still check online status
            // with the same guards: skip local: zones, skip zones without a device,
            // and allow if device is still present in the live OutputRegistry.
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();
            if let Some(ref zone) = zone_db {
                if !zone.online {
                    let dev_id = zone.output_device_id.as_deref().unwrap_or("");
                    let is_local = dev_id.starts_with("local:");
                    let has_device = !dev_id.is_empty();
                    let in_registry =
                        has_device && !is_local && self.outputs.lock().await.contains(dev_id);
                    if has_device && !is_local && !in_registry {
                        let msg = format!("Output device offline: {}", zone.name);
                        warn!(zone_id = req.zone_id, zone_name = %zone.name, "play_rejected_zone_offline");
                        if let Some(ref bus) = self.event_bus {
                            bus.emit(
                                "zone.playback_error",
                                serde_json::json!({
                                    "zone_id": req.zone_id,
                                    "error": msg,
                                }),
                            );
                        }
                        return Err(msg);
                    }
                }
            }
        }

        // Clean up any gapless-prepared session for this zone before
        // creating a new stream.
        self.cleanup_gapless_session(req.zone_id).await;

        // Remember old session for cleanup AFTER output has been stopped
        let prev_state = self.playback.get_state(req.zone_id).await;
        let old_stream_id = prev_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());

        // Bump track_generation NOW so the poller resets its wall-clock
        // timer immediately. Without this, a long DASH transcode (20-30s)
        // can run into the 300s timeout from the previous track.
        self.playback.bump_generation(req.zone_id).await;

        let resolved = if let Some(ref temp_path) = req.temp_file_path {
            self.resolve_uploaded_file(temp_path, &req).await?
        } else {
            self.resolve_stream(&req).await?
        };
        let resolve_ms = play_start.elapsed().as_millis();

        let cover_path = req.cover_url.clone().or(resolved.cover_url.clone());
        let album = req.album_title.clone().or(resolved.album.clone());
        let track_meta = req.track_id.and_then(|tid| {
            crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                .get(tid)
                .ok()
                .flatten()
        });
        let np = NowPlaying {
            track_id: req.track_id,
            title: resolved.title.clone(),
            artist_name: resolved.artist.clone(),
            album_title: album.clone(),
            cover_path: cover_path.clone(),
            duration_ms: resolved.duration_ms.unwrap_or(0),
            source: resolved.source.clone(),
            source_id: req.source_id.clone(),
            stream_id: resolved.stream_id.clone(),
            format: track_meta
                .as_ref()
                .and_then(|t| t.format.clone())
                .or_else(|| {
                    let mime = &resolved.mime_type;
                    Some(
                        mime.strip_prefix("audio/")
                            .unwrap_or(mime)
                            .replace("x-", "")
                            .to_string(),
                    )
                }),
            sample_rate: resolved.sample_rate.or(track_meta
                .as_ref()
                .and_then(|t| t.sample_rate.map(|v| v as u32))),
            bit_depth: resolved.bit_depth.or(track_meta
                .as_ref()
                .and_then(|t| t.bit_depth.map(|v| v as u32))),
            genre: track_meta.as_ref().and_then(|t| t.genre.clone()),
            year: track_meta.as_ref().and_then(|t| t.year),
        };

        self.playback.play(req.zone_id, np).await;

        // Persist play state for auto-resume after server restart
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(req.zone_id, "playing")
            .ok();

        // Multi-service now-playing dispatch with tier gating
        self.dispatch_now_playing(
            &resolved.title,
            resolved.artist.as_deref(),
            album.as_deref(),
        );

        // For local outputs, keep the old stream alive until after play_url()
        // calls stop() — otherwise the audio thread gets a read error when the
        // HTTP session is removed while it's still reading. For network outputs
        // (DLNA), close the old stream first to avoid stale bytes.
        let is_local = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        if !is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        let (output_sent, output_error) = if let Some(ref device_id) = req.output_device_id {
            let resolved_cover_url = self.resolve_cover_url(cover_path.as_deref());
            let local_file_path = if resolved.source == "local" {
                req.track_id.and_then(|tid| {
                    TrackRepo::with_backend(self.db.clone())
                        .get(tid)
                        .ok()
                        .flatten()
                        .and_then(|t| t.file_path)
                })
            } else {
                None
            };
            let media = crate::outputs::traits::PlayMedia {
                url: &resolved.url,
                mime_type: &resolved.mime_type,
                title: Some(&resolved.title),
                artist: resolved.artist.as_deref(),
                album: album.as_deref(),
                cover_url: resolved_cover_url.as_deref(),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                file_size: resolved.file_size,
                file_path: local_file_path.as_deref(),
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
            };
            let result = self.send_to_output(device_id, &media, req.seek_ms).await;
            let total_ms = play_start.elapsed().as_millis();
            info!(
                zone_id = req.zone_id,
                resolve_ms,
                output_ms = total_ms.saturating_sub(resolve_ms),
                total_ms,
                title = %resolved.title,
                "playback_timing"
            );

            // After play_media succeeds, send the zone's stored volume to the
            // renderer — but ONLY if the user has explicitly set a volume
            // (not the default 50). This prevents blasting speakers at an
            // unexpected level after a server restart.
            if result.0 {
                let zone_db = ZoneRepo::with_backend(self.db.clone())
                    .get(req.zone_id)
                    .ok()
                    .flatten();
                let db_volume = zone_db.as_ref().map(|z| z.volume).unwrap_or(50);
                let is_fixed = zone_db.as_ref().is_some_and(|z| z.fixed_volume);
                let zone_volume = if is_fixed {
                    1.0
                } else {
                    let ps = self.playback.get_state(req.zone_id).await;
                    if ps.volume > 0.0 {
                        ps.volume
                    } else {
                        db_volume as f64 / 100.0
                    }
                };
                let did = device_id.clone();
                let outputs = self.outputs.clone();
                let zone_id = req.zone_id;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let outputs = outputs.lock().await;
                    if let Some(output) = outputs.get(&did) {
                        let vol_clamped = zone_volume.clamp(0.0, 1.0);
                        if let Err(e) = output.lock().await.set_volume(vol_clamped).await {
                            warn!(zone_id, volume = %vol_clamped, error = %e, "play_initial_volume_failed");
                        } else {
                            info!(zone_id, volume = %vol_clamped, "play_initial_volume_sent");
                        }
                    }
                });
            }

            result
        } else {
            warn!(
                zone_id = req.zone_id,
                "no_output_device_id_skipping_send_to_output"
            );
            (false, None)
        };

        // For local outputs, clean up the old stream now that play_url() has
        // called stop() and the old audio thread is no longer reading.
        if is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        self.record_listen(
            &resolved.title,
            resolved.artist.as_deref(),
            album.as_deref(),
            &resolved.source,
            req.source_id.as_deref(),
            req.track_id.and_then(|tid| {
                TrackRepo::with_backend(self.db.clone())
                    .get(tid)
                    .ok()
                    .flatten()
                    .and_then(|t| t.album_id)
            }),
            resolved.duration_ms.unwrap_or(0),
            req.zone_id,
            cover_path.as_deref(),
        );

        info!(
            zone_id = req.zone_id,
            title = %resolved.title,
            source = %resolved.source,
            output_sent,
            "orchestrator_play"
        );

        // Trigger prefetch of the next track in the background.
        // This runs concurrently with the current playback so the next
        // streaming track is already decoded in memory when needed.
        {
            let prefetch = self.prefetch.clone();
            let db = self.db.clone();
            let services = self.services.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            tokio::spawn(async move {
                prefetch
                    .prefetch_next(db, services, playback, zone_id)
                    .await;
            });
        }

        Ok(PlayResult {
            stream_url: Some(resolved.url),
            output_sent,
            source: resolved.source,
            error: output_error,
        })
    }

    /// Check whether a DLNA renderer supports a given MIME type by querying
    /// its ConnectionManager GetProtocolInfo Sink.  Results are cached per
    /// device_id so the SOAP call only happens once per renderer per session.
    async fn dlna_supports_mime(&self, device_id: &str, mime: &str) -> bool {
        // Check negative cache first
        {
            let cache = self.dlna_unsupported_mimes.lock().await;
            if let Some(unsupported) = cache.get(device_id) {
                if unsupported.iter().any(|m| m == mime) {
                    return false;
                }
                // We already probed this device — if the MIME is not in the
                // unsupported list, it means it was supported.
                if !unsupported.is_empty() {
                    // Device was probed at least once (it returned some
                    // unsupported entries or we stored an empty vec for it).
                    // But we can't distinguish "probed and supported" from
                    // "never checked this mime".  So we only use the cache
                    // for known negatives and re-probe below if needed.
                }
            }
        }

        // Probe the renderer
        let supported = {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(device_id) {
                let locked = output.lock().await;
                if let Some(dlna) = locked
                    .as_any()
                    .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                {
                    dlna.supports_mime(mime).await
                } else {
                    // Not a DLNA output — format negotiation doesn't apply
                    true
                }
            } else {
                true
            }
        };

        if !supported {
            let mut cache = self.dlna_unsupported_mimes.lock().await;
            let entry = cache.entry(device_id.to_string()).or_default();
            if !entry.iter().any(|m| m == mime) {
                entry.push(mime.to_string());
            }
        }

        supported
    }

    async fn should_dsd_passthrough(&self, zone_id: i64, device_id: &str) -> bool {
        let dsd_mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        match dsd_mode.as_str() {
            "pcm" => false,
            "native" => true,
            _ => {
                // Auto mode: probe renderer
                let mut cache = self.dsd_capabilities.lock().await;
                if let Some(cap) = cache.get(device_id) {
                    return cap.supports_dsf || cap.supports_dff;
                }
                let cap = {
                    let outputs = self.outputs.lock().await;
                    if let Some(output) = outputs.get(device_id) {
                        let locked = output.lock().await;
                        if let Some(dlna) = locked
                            .as_any()
                            .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                        {
                            dlna.probe_dsd_support().await
                        } else {
                            crate::outputs::dlna::DsdCapability::default()
                        }
                    } else {
                        crate::outputs::dlna::DsdCapability::default()
                    }
                };
                let result = cap.supports_dsf || cap.supports_dff;
                cache.insert(device_id.to_string(), cap);
                result
            }
        }
    }

    async fn resolve_uploaded_file(
        &self,
        file_path: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            return Err(format!("uploaded file not found: {file_path}"));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("wav")
            .to_lowercase();
        let format = crate::audio::formats::AudioFormat::from_extension(&ext);
        let meta = crate::metadata::try_read_metadata(path);
        let title = req
            .title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.title.clone()))
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            });
        let artist = req
            .artist_name
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.artist.clone()));
        let album = req
            .album_title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.album.clone()));
        let duration_ms = req
            .duration_ms
            .map(|d| d as u64)
            .or_else(|| meta.as_ref().ok().and_then(|m| m.duration_ms))
            .unwrap_or(0);
        let sample_rate = meta.as_ref().ok().and_then(|m| m.sample_rate);
        let bit_depth = meta.as_ref().ok().and_then(|m| m.bit_depth);
        let channels = meta.as_ref().ok().and_then(|m| m.channels).unwrap_or(2);

        let mime = format
            .as_ref()
            .map(|f| f.mime_type())
            .unwrap_or("audio/wav")
            .to_string();
        let file_size = std::fs::metadata(path).ok().map(|m| m.len());

        let info = StreamInfo {
            format: ext.clone(),
            mime_type: mime.clone(),
            sample_rate: sample_rate.unwrap_or(44100) as u32,
            bit_depth: bit_depth.unwrap_or(16),
            channels: channels as u16,
            file_size,
            duration_ms: Some(duration_ms as u64),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(info, true, 128).await;
        let fp = file_path.to_string();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let file = std::fs::read(&fp);
            match file {
                Ok(data) => {
                    let _ = rt.block_on(tx.send(data));
                    data_ready.notify_one();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "uploaded_file_read_failed");
                }
            }
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, &ext);

        Ok(ResolvedStream {
            url: stream_url,
            stream_id: Some(session_id),
            title,
            artist,
            album,
            duration_ms: Some(duration_ms as i64),
            source: "upload".into(),
            mime_type: mime,
            sample_rate: sample_rate.map(|s| s as u32),
            bit_depth: bit_depth.map(|b| b as u32),
            channels: Some(channels as u32),
            cover_url: None,
            file_size,
        })
    }

    async fn resolve_stream(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        if let Some(ref source) = req.source
            && source != "local"
        {
            if source == "podcast" || source == "radio" || source == "upnp" {
                return self.resolve_direct_url(req).await;
            }
            return self.resolve_streaming_url(source, req).await;
        }

        self.resolve_local_track(req).await
    }

    async fn resolve_direct_url(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        let audio_url = req
            .source_id
            .as_deref()
            .ok_or("source_id (audio URL) required for podcast/radio playback")?;
        let title = req.title.clone().unwrap_or_else(|| "Episode".into());
        let artist = req.artist_name.clone();
        let album = req.album_title.clone();
        let cover_url = req.cover_url.clone();
        let duration_ms = req.duration_ms;
        let source = req.source.clone().unwrap_or_else(|| "podcast".into());
        let mime_type = guess_mime_from_url(audio_url);
        let is_radio = source == "radio";

        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));

        let (url, stream_id, out_mime, out_sr, out_bd, out_ch) =
            if is_radio && (is_local_output || is_oaat_output) {
                // Local/OAAT outputs cannot play compressed streams directly —
                // they expect raw PCM in a WAV container.  For radio (infinite
                // stream), we decode the HTTP stream progressively to PCM and
                // serve it as WAV through a streaming session.
                let wav_info = StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 44100,
                    bit_depth: 16,
                    channels: 2,
                    file_size: None,
                    duration_ms: None,
                    ..Default::default()
                };

                let (session_id, tx, data_ready) =
                    self.streamer.create_radio_session(wav_info, 256).await;

                info!(
                    source = "radio",
                    url = %audio_url,
                    "radio_decode_to_wav_for_local_output"
                );

                let radio_url = audio_url.to_string();
                tokio::spawn(async move {
                    // Download + decode in a blocking thread since symphonia and
                    // reqwest::blocking are both synchronous.
                    let result = tokio::task::spawn_blocking(move || {
                        decode_radio_stream_to_pcm(radio_url, tx, data_ready)
                    })
                    .await;

                    match result {
                        Ok(Ok(())) => {
                            debug!("radio_local_decode_stream_ended");
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "radio_local_decode_failed");
                        }
                        Err(e) => {
                            warn!(error = %e, "radio_local_decode_task_panic");
                        }
                    }
                });

                let server_ip = self.server_ip();
                let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                (
                    stream_url,
                    Some(session_id),
                    "audio/wav".to_string(),
                    Some(44100u32),
                    Some(16u32),
                    Some(2u32),
                )
            } else if is_radio {
                // Network outputs (DLNA): check if the renderer supports the
                // radio stream format (typically AAC). If not, proxy + transcode
                // to WAV so the renderer can play it.
                let needs_proxy = if let Some(device_id) = req.output_device_id.as_deref() {
                    let radio_mime = guess_mime_from_url(audio_url);
                    !self.dlna_supports_mime(device_id, &radio_mime).await
                } else {
                    false
                };

                if needs_proxy {
                    let wav_info = StreamInfo {
                        format: "wav".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: 44100,
                        bit_depth: 16,
                        channels: 2,
                        file_size: None,
                        duration_ms: None,
                        ..Default::default()
                    };
                    let (session_id, tx, data_ready) =
                        self.streamer.create_radio_session(wav_info, 256).await;
                    info!(url = %audio_url, "radio_proxy_transcode_for_dlna");
                    let radio_url = audio_url.to_string();
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            decode_radio_stream_to_pcm(radio_url, tx, data_ready)
                        })
                        .await;
                        match result {
                            Ok(Ok(())) => debug!("radio_dlna_decode_stream_ended"),
                            Ok(Err(e)) => warn!(error = %e, "radio_dlna_decode_failed"),
                            Err(e) => warn!(error = %e, "radio_dlna_decode_task_panic"),
                        }
                    });
                    let server_ip = self.server_ip();
                    let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                    (
                        stream_url,
                        Some(session_id),
                        "audio/wav".to_string(),
                        Some(44100u32),
                        Some(16u32),
                        Some(2u32),
                    )
                } else {
                    // Renderer supports the format — send direct URL.
                    // Downgrade https→http since DLNA renderers can't do TLS.
                    let direct_url = if audio_url.starts_with("https://") {
                        audio_url.replacen("https://", "http://", 1)
                    } else {
                        audio_url.to_string()
                    };
                    (direct_url, None, mime_type.to_string(), None, None, None)
                }
            } else {
                (
                    audio_url.to_string(),
                    None,
                    mime_type.to_string(),
                    None,
                    None,
                    None,
                )
            };

        Ok(ResolvedStream {
            url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source,
            cover_url,
            stream_id,
            file_size: None,
            sample_rate: out_sr,
            bit_depth: out_bd,
            channels: out_ch,
        })
    }

    async fn resolve_local_track(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        let track_id = req.track_id.ok_or("no track_id for local playback")?;
        let repo = TrackRepo::with_backend(self.db.clone());
        let track = repo
            .get(track_id)
            .map_err(|e| e.to_string())?
            .ok_or("track not found")?;

        let file_path = track.file_path.ok_or("track has no file_path")?;
        let fmt = track.format.unwrap_or_else(|| "flac".into());
        let source_format = AudioFormat::from_extension(&fmt);
        let sample_rate = track.sample_rate.unwrap_or(44100) as u32;
        let bit_depth = track.bit_depth.unwrap_or(16) as u16;
        let channels = track.channels as u16;

        // Determine the output type and max_sample_rate for this zone.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(req.zone_id)
            .ok()
            .flatten();
        let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
        let zone_max_sample_rate = zone.as_ref().and_then(|z| z.max_sample_rate);

        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        // OAAT endpoints: transcode to WAV for reliable bit-perfect playback.
        // Always transcode, even WAV sources, to normalise EXTENSIBLE/FLOAT
        // variants into simple PCM that the endpoint can reliably parse.
        let oaat_needs_wav = is_oaat_output && source_format.is_some();

        // Local output (cpal) has a simple WAV parser that only understands
        // standard PCM (format tag 1).  Real-world WAV files can use
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE), IEEE_FLOAT (3), or have extra
        // metadata chunks that shift the data offset beyond the parser's
        // 4096-byte header buffer.  Feeding such files as passthrough causes
        // white noise because the byte layout doesn't match what the parser
        // expects (wrong bit depth, wrong data offset, or float-as-integer).
        //
        // Fix: ALWAYS transcode through symphonia for local output, even when
        // the source is already WAV.  Symphonia handles all WAV variants and
        // produces normalised integer PCM.  The HTTP stream handler then
        // prepends a simple 44-byte PCM header that the local parser handles
        // correctly.  The overhead is negligible (memcpy, no re-encoding).
        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let local_needs_wav = is_local_output && source_format.is_some();

        // DSD DoP (DSD over PCM) for local output when dsd_mode is "native"
        if is_local_output && source_format == Some(AudioFormat::Dsd) {
            let dsd_mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(req.zone_id);
            if dsd_mode == "native" || dsd_mode == "dop" {
                let dsd_rate = track.sample_rate.unwrap_or(2_822_400) as u32;
                let mut dop_rate = crate::audio::dsd_to_dop::DsdToDoP::dop_rate(dsd_rate);
                let zone_max_sr = zone.as_ref().and_then(|z| z.max_sample_rate);
                if let Some(max_sr) = zone_max_sr {
                    if dop_rate > max_sr {
                        info!(
                            dsd_rate,
                            dop_rate, max_sr, "dsd_dop_rate_exceeds_zone_max_falling_back_to_pcm"
                        );
                        // Fall through to normal DSD→PCM transcode path
                    }
                }
                if zone_max_sr.is_none_or(|max_sr| dop_rate <= max_sr) {
                    let dop_channels = track.channels.max(2) as u16;

                    let wav_info = StreamInfo {
                        format: "wav".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: dop_rate,
                        bit_depth: 24,
                        channels: dop_channels,
                        file_size: None,
                        duration_ms: Some(track.duration_ms as u64),
                        ..Default::default()
                    };

                    let (session_id, tx, data_ready) =
                        self.streamer.create_session(wav_info, true, 128).await;

                    info!(
                        file = %file_path,
                        dsd_rate,
                        dop_rate,
                        channels = dop_channels,
                        "dsd_dop_streaming_for_local_output"
                    );

                    let fp = file_path.clone();
                    let ext = std::path::Path::new(&fp)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("dsf")
                        .to_lowercase();
                    tokio::task::spawn_blocking(move || {
                        // Send WAV header first
                        let wav_hdr =
                            crate::audio::wav::build_wav_header(dop_channels, dop_rate, 24);
                        let rt = tokio::runtime::Handle::current();
                        let _ = rt.block_on(tx.send(wav_hdr.to_vec()));
                        data_ready.notify_one();

                        let mut first = false;
                        match crate::audio::decode::decode_dsd_to_dop_streaming(
                            &fp, &ext, tx, 65536, &mut first, &None, &rt,
                        ) {
                            Ok(_) => tracing::debug!("dsd_dop_stream_complete"),
                            Err(e) => tracing::warn!(error = %e, "dsd_dop_stream_failed"),
                        }
                    });

                    let server_ip = self.server_ip();
                    let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

                    return Ok(ResolvedStream {
                        url: stream_url,
                        stream_id: Some(session_id),
                        title: track.title.clone(),
                        artist: track.artist_name.clone(),
                        album: track.album_title.clone(),
                        duration_ms: Some(track.duration_ms),
                        source: "local".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: Some(dop_rate),
                        bit_depth: Some(24),
                        channels: Some(dop_channels as u32),
                        cover_url: self.resolve_cover_url(track.cover_path.as_deref()),
                        file_size: None,
                    });
                } // end dop_rate <= max check
            }
        }

        // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC, WMA) for network outputs
        // that receive a URL and play it directly. FLAC, WAV, MP3, AAC pass through as-is.
        let is_network_output = matches!(
            zone_output_type.as_deref(),
            Some("dlna")
                | Some("openhome")
                | Some("chromecast")
                | Some("bluos")
                | Some("squeezebox")
        );

        // DSD native passthrough: skip transcode when the renderer supports DSD natively.
        let dsd_passthrough = if source_format == Some(AudioFormat::Dsd) && is_network_output {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            self.should_dsd_passthrough(req.zone_id, did).await
        } else {
            false
        };

        let needs_transcode_for_output = is_network_output
            && !dsd_passthrough
            && source_format
                .as_ref()
                .is_some_and(|f| f.needs_transcode_for_dlna());

        // DLNA format negotiation: if the output will be FLAC (either source
        // is FLAC, or source needs transcode and target is FLAC), check that
        // the renderer supports audio/flac. Otherwise force WAV (LPCM).
        let is_dlna = zone_output_type.as_deref() == Some("dlna");
        let will_be_flac = source_format == Some(AudioFormat::Flac)
            || (needs_transcode_for_output
                && source_format
                    .map(|f| f.dlna_transcode_target() == AudioFormat::Flac)
                    .unwrap_or(false));
        let dlna_needs_wav = if is_dlna && will_be_flac {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            if !did.is_empty() {
                !self.dlna_supports_mime(did, "audio/flac").await
            } else {
                false
            }
        } else {
            false
        };

        // Downsample if the zone has a max_sample_rate cap and the source exceeds it
        let needs_downsample = zone_max_sample_rate.is_some_and(|max| sample_rate > max);
        let needs_transcode = needs_transcode_for_output
            || oaat_needs_wav
            || local_needs_wav
            || needs_downsample
            || dlna_needs_wav;

        let (
            session_id,
            out_mime,
            out_ext,
            resolved_file_size,
            resolved_sr,
            resolved_bd,
            resolved_ch,
        ) = if needs_transcode {
            let src_fmt = source_format.unwrap_or(AudioFormat::Flac);
            let target_fmt = if oaat_needs_wav || local_needs_wav {
                AudioFormat::Wav
            } else if dlna_needs_wav {
                // Renderer doesn't support FLAC — transcode to WAV (LPCM)
                // which has a proper DLNA.ORG_PN=LPCM profile.
                AudioFormat::Wav
            } else if needs_downsample && !needs_transcode_for_output {
                // Only downsampling — keep the same lossless format
                AudioFormat::Flac
            } else {
                src_fmt.dlna_transcode_target()
            };
            let mut out_sr = src_fmt.dsd_output_sample_rate(sample_rate);
            // Apply zone max_sample_rate cap
            if let Some(max_sr) = zone_max_sample_rate {
                if out_sr > max_sr {
                    info!(
                        zone_id = req.zone_id,
                        source_rate = out_sr,
                        max_rate = max_sr,
                        "zone_max_sample_rate_cap_applied"
                    );
                    out_sr = max_sr;
                }
            }
            let out_bd: u16 = if local_needs_wav {
                // Local output (cpal/WASAPI): always use 32-bit WAV.
                //
                // Symphonia decodes all audio into AudioBuffer<i32> (left-justified
                // 32-bit integers) regardless of source bit depth.  When packing
                // these into 24-bit (3 bytes/sample), any mismatch between the
                // reported source_bd and the actual sample range causes byte
                // misalignment in the PCM stream — the local parser then reads
                // from wrong offsets, producing white noise.
                //
                // Using 32-bit eliminates this class of bugs entirely: each i32
                // sample is written as 4 bytes, matching the WAV header's declared
                // byte width.  The local output converts to f32 for cpal anyway,
                // so there is zero quality loss.
                32
            } else if src_fmt == AudioFormat::Dsd {
                24
            } else if oaat_needs_wav || dlna_needs_wav {
                // OAAT endpoints and DLNA renderers that need WAV fallback:
                // cap at 24-bit (LPCM max for most DLNA renderers).
                bit_depth.max(16).min(24)
            } else if src_fmt == AudioFormat::Alac {
                // ALAC: transcode to FLAC for DLNA (universally supported).
                // FLAC max is 24-bit; cap at min(source_bd, 24) but at least 16.
                bit_depth.min(24).max(16)
            } else {
                bit_depth.max(16)
            };
            let out_mime = if oaat_needs_wav || local_needs_wav {
                "audio/wav".to_string()
            } else {
                target_fmt.mime_type().to_string()
            };
            let out_ext = if oaat_needs_wav || local_needs_wav {
                "wav".to_string()
            } else {
                target_fmt.container_format().to_string()
            };

            info!(
                file = %file_path,
                source = ?src_fmt,
                target = ?target_fmt,
                sample_rate = out_sr,
                bit_depth = out_bd,
                "transcode_required"
            );

            // For network outputs (DLNA, OpenHome, etc.) with non-WAV targets
            // (e.g. FLAC), pre-transcode to a temp file on disk so the HTTP
            // handler can serve it with Content-Length and Accept-Ranges.
            // Renderers like the darTZeel LHC-208 reject chunked transfer
            // (no Content-Length) and require a known file size.
            //
            // For local/OAAT outputs (WAV target), keep using streaming
            // sessions — those outputs don't need Content-Length.
            let target_format_str = if target_fmt == AudioFormat::Wav {
                "wav".to_string()
            } else {
                target_fmt.container_format().to_string()
            };
            // Network outputs need file transcode for Content-Length + Range.
            // Local outputs use streaming sessions — the _keep_alive_tx in
            // StreamSession prevents the channel from closing when the decoder
            // finishes, so ASIO/WASAPI can consume all buffered data at their
            // own pace. This avoids the 28s download delay of file transcode.
            let use_file_transcode =
                is_network_output && (target_format_str != "wav" || dlna_needs_wav);

            let info = StreamInfo {
                format: out_ext.clone(),
                mime_type: out_mime.clone(),
                sample_rate: out_sr,
                bit_depth: out_bd,
                channels,
                file_size: None,
                duration_ms: Some(track.duration_ms as u64),
                ..Default::default()
            };

            if use_file_transcode {
                // ── Pre-transcode to temp file (FLAC) ──────────────────
                // Decode → encode → write to /tmp, then create a file session.
                // The HTTP handler serves file sessions with Content-Length
                // and Range support, which DLNA renderers require.
                let fp = file_path.clone();
                let ev_bus = self.event_bus.clone();
                let zone_id = req.zone_id;
                let tmp_path = std::env::temp_dir()
                    .join(format!(
                        "tune-transcode-{}.{}",
                        uuid::Uuid::new_v4(),
                        &out_ext
                    ))
                    .to_string_lossy()
                    .to_string();

                info!(
                    file = %fp,
                    tmp = %tmp_path,
                    target = %target_format_str,
                    sample_rate = out_sr,
                    bit_depth = out_bd,
                    "transcode_to_temp_file_start"
                );

                let tmp_path_clone = tmp_path.clone();
                let target_fmt_str = target_format_str.clone();
                let eq_profile = self.load_eq_processor(req.zone_id, out_sr, channels);
                let transcode_result =
                    tokio::time::timeout(std::time::Duration::from_secs(120), async move {
                        // 1. Decode source to PCM (blocking I/O)
                        let fp_clone = fp.clone();
                        let decoded = tokio::task::spawn_blocking(move || {
                            crate::audio::decode::decode_to_pcm(
                                &fp_clone,
                                Some(out_sr),
                                Some(channels as u32),
                                0.0,
                                0.0,
                            )
                        })
                        .await
                        .map_err(|e| format!("decode task panic: {e}"))??;

                        let mut pcm_bytes = decoded.pcm_bytes();
                        let actual_bd = decoded.bit_depth;

                        // 1b. Apply EQ if enabled for this zone
                        if let Some(mut eq) = eq_profile {
                            eq.process_pcm(&mut pcm_bytes, actual_bd);
                        }

                        // 2. Encode to target format (async — no block_on needed)
                        let mut encoder = crate::audio::encoder::AudioEncoder::new(
                            &target_fmt_str,
                            decoded.sample_rate,
                            actual_bd as u32,
                            decoded.channels,
                        );
                        encoder.start().await?;
                        encoder.write(&pcm_bytes).await?;
                        let encoded_data = encoder.finish().await?;

                        // 3. Write to temp file (blocking I/O)
                        let tmp_write = tmp_path_clone.clone();
                        let encoded_clone = encoded_data.clone();
                        tokio::task::spawn_blocking(move || {
                            std::fs::write(&tmp_write, &encoded_clone)
                                .map_err(|e| format!("write temp file: {e}"))
                        })
                        .await
                        .map_err(|e| format!("write task panic: {e}"))??;

                        let file_size = encoded_data.len() as u64;
                        Ok::<(u64, Vec<u8>, u16), String>((file_size, pcm_bytes, actual_bd))
                    })
                    .await;

                match transcode_result {
                    Ok(Ok((file_size, pcm_bytes, actual_bd))) => {
                        if file_size < 1024 {
                            warn!(
                                file = %file_path,
                                file_size,
                                "transcode_produced_empty_file — source may be corrupted or encrypted"
                            );
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err("transcode produced empty file (corrupted source?)".into());
                        }
                        info!(
                            file = %file_path,
                            tmp = %tmp_path,
                            file_size,
                            "transcode_to_temp_file_complete"
                        );

                        // Emit audio levels in the background
                        if let Some(ref bus) = ev_bus {
                            let bus = bus.clone();
                            let actual_ch = channels;
                            let sr = out_sr;
                            tokio::spawn(async move {
                                let (levels_tx, mut levels_rx) =
                                    tokio::sync::mpsc::unbounded_channel::<
                                        crate::audio::levels::AudioLevels,
                                    >();
                                let bus_clone = bus.clone();
                                tokio::spawn(async move {
                                    while let Some(lvl) = levels_rx.recv().await {
                                        bus_clone.emit(
                                            "playback.audio_levels",
                                            serde_json::json!({
                                                "zone_id": zone_id,
                                                "rms_left_db": lvl.rms_left_db(),
                                                "rms_right_db": lvl.rms_right_db(),
                                                "peak_left_db": lvl.peak_left_db(),
                                                "peak_right_db": lvl.peak_right_db(),
                                                "rms_left": lvl.rms_left,
                                                "rms_right": lvl.rms_right,
                                                "spectrum": lvl.spectrum,
                                            }),
                                        );
                                    }
                                });
                                tokio::task::spawn_blocking(move || {
                                    for chunk in pcm_bytes.chunks(32768) {
                                        if levels_tx
                                            .send(crate::audio::levels::compute_levels(
                                                chunk, actual_bd, actual_ch, sr,
                                            ))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                })
                                .await
                                .ok();
                            });
                        }

                        // Create a file session — HTTP handler serves with
                        // Content-Length and Range support.
                        let file_info = StreamInfo {
                            format: out_ext.clone(),
                            mime_type: out_mime.clone(),
                            sample_rate: out_sr,
                            bit_depth: out_bd,
                            channels,
                            file_size: Some(file_size),
                            duration_ms: Some(track.duration_ms as u64),
                            ..Default::default()
                        };
                        let session_id = self
                            .streamer
                            .create_file_session(file_info, tmp_path, false)
                            .await;

                        (
                            session_id,
                            out_mime,
                            out_ext,
                            Some(file_size),
                            Some(out_sr),
                            Some(out_bd as u32),
                            Some(channels as u32),
                        )
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, file = %file_path, "transcode_to_temp_file_failed");
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(format!("transcode failed: {e}"));
                    }
                    Err(_) => {
                        warn!(file = %file_path, "transcode_timeout_120s");
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(
                            "transcode timeout (120s) — file too large or I/O stalled".into()
                        );
                    }
                }
            } else {
                // ── Streaming transcode (WAV for local/OAAT) ──────────
                // Use the computed WAV content length for the DIDL size
                // attribute so DLNA renderers know the correct stream size.
                let transcode_file_size = info.wav_content_length();

                let (session_id, tx, data_ready) =
                    self.streamer.create_session(info, false, 256).await;

                // Mark session: the streaming decoder sends the WAV header
                // with the real source sample rate, so the stream handler
                // must NOT prepend its own.
                {
                    let sessions = self.streamer.sessions_state();
                    let sessions = sessions.lock().await;
                    if let Some(session) = sessions.get(&session_id) {
                        session
                            .wav_header_included
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                let fp = file_path.clone();
                let ev_bus = self.event_bus.clone();
                let zone_id = req.zone_id;
                let seek_s = req.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);
                let streamer_sessions = self.streamer.sessions_state();
                let close_session_id = session_id.clone();
                tokio::spawn(async move {
                    debug!(file = %fp, sample_rate = out_sr, channels, "transcode_decoding");

                    let (levels_tx, mut levels_rx) =
                        tokio::sync::mpsc::unbounded_channel::<crate::audio::levels::AudioLevels>();
                    if let Some(ref bus) = ev_bus {
                        let bus = bus.clone();
                        tokio::spawn(async move {
                            while let Some(lvl) = levels_rx.recv().await {
                                bus.emit(
                                    "playback.audio_levels",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "rms_left_db": lvl.rms_left_db(),
                                        "rms_right_db": lvl.rms_right_db(),
                                        "peak_left_db": lvl.peak_left_db(),
                                        "peak_right_db": lvl.peak_right_db(),
                                        "rms_left": lvl.rms_left,
                                        "rms_right": lvl.rms_right,
                                        "spectrum": lvl.spectrum,
                                    }),
                                );
                            }
                        });
                    }

                    let fp_clone = fp.clone();
                    let tx_clone = tx.clone();
                    drop(tx);

                    let result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &fp_clone,
                            Some(out_sr),
                            Some(channels as u32),
                            Some(out_bd),
                            tx_clone,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await;

                    match result {
                        Ok(Ok(_bit_depth)) => {
                            debug!(file = %fp, "transcode_complete_streaming");
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, file = %fp, "transcode_streaming_decode_failed");
                        }
                        Err(e) => {
                            warn!(error = %e, file = %fp, "transcode_streaming_task_panic");
                        }
                    }

                    // Signal EOF by dropping the keep-alive sender. The
                    // decoder's tx is already dropped at this point, but the
                    // _keep_alive_tx in the session keeps the channel open
                    // until we explicitly close it here.
                    let sessions = streamer_sessions.lock().await;
                    if let Some(session) = sessions.get(&close_session_id) {
                        session.close_sender().await;
                    }
                });

                (
                    session_id,
                    out_mime,
                    out_ext,
                    transcode_file_size,
                    Some(out_sr),
                    Some(out_bd as u32),
                    Some(channels as u32),
                )
            }
        } else {
            // Standard passthrough: serve the raw file.
            // For DSD, use the MIME type declared by the renderer (from GetProtocolInfo)
            // instead of the generic application/x-dsd — some renderers (Yamaha R-N2000A)
            // only accept the specific MIME they advertise (e.g. audio/dsf).
            let mime = if source_format == Some(AudioFormat::Dsd) && is_network_output {
                let did = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let cap = self.dsd_capabilities.lock().await;
                cap.get(did)
                    .and_then(|c| c.dsf_mime.clone())
                    .unwrap_or_else(|| "application/x-dsd".into())
            } else {
                source_format
                    .map(|f| f.mime_type().to_string())
                    .unwrap_or_else(|| "audio/flac".into())
            };

            let info = StreamInfo {
                format: fmt.clone(),
                mime_type: mime.clone(),
                sample_rate,
                bit_depth,
                channels,
                file_size: track.file_size.map(|s| s as u64),
                duration_ms: Some(track.duration_ms as u64),
                ..Default::default()
            };

            let passthrough_file_size = track.file_size.map(|s| s as u64);

            let session_id = self
                .streamer
                .create_file_session(info, file_path.clone(), false)
                .await;

            // Parallel decode-for-levels: decode the audio in the background
            // purely to emit VU-meter events for the web client. This does not
            // affect the actual audio stream served to the output device.
            // Skip DSD (1-bit at MHz rates, can't decode inline for levels)
            // and exotic formats that need heavy conversion.
            let skip_passthrough_levels = source_format
                .as_ref()
                .is_some_and(|f| f.needs_transcode_for_dlna());
            if !skip_passthrough_levels {
                if let Some(ref bus) = self.event_bus {
                    let bus = bus.clone();
                    let fp = file_path.clone();
                    let zone_id = req.zone_id;
                    let sr = sample_rate;
                    let ch = channels as u32;
                    tokio::spawn(async move {
                        let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel::<
                            crate::audio::levels::AudioLevels,
                        >();
                        let bus_clone = bus.clone();
                        tokio::spawn(async move {
                            while let Some(lvl) = levels_rx.recv().await {
                                bus_clone.emit(
                                    "playback.audio_levels",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "rms_left_db": lvl.rms_left_db(),
                                        "rms_right_db": lvl.rms_right_db(),
                                        "peak_left_db": lvl.peak_left_db(),
                                        "peak_right_db": lvl.peak_right_db(),
                                        "rms_left": lvl.rms_left,
                                        "rms_right": lvl.rms_right,
                                    }),
                                );
                            }
                        });
                        // Decode the file to PCM in the background — output is
                        // discarded, only levels are forwarded via levels_tx.
                        let result = tokio::task::spawn_blocking(move || {
                            let decoded = crate::audio::decode::decode_to_pcm(
                                &fp,
                                Some(sr),
                                Some(ch),
                                0.0,
                                0.0,
                            );
                            if let Ok(ref dec) = decoded {
                                let pcm = dec.pcm_bytes();
                                let bd = dec.bit_depth;
                                let c = dec.channels as u16;
                                for chunk in pcm.chunks(32768) {
                                    if levels_tx
                                        .send(crate::audio::levels::compute_levels(
                                            chunk, bd, c, sr,
                                        ))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        })
                        .await;
                        if let Err(e) = result {
                            debug!(error = %e, "passthrough_levels_task_panic");
                        }
                    });
                }
            }

            (
                session_id,
                mime,
                fmt.clone(),
                passthrough_file_size,
                Some(sample_rate),
                Some(bit_depth as u32),
                Some(channels as u32),
            )
        };

        let server_ip = self.server_ip();
        let stream_url = self
            .streamer
            .get_stream_url(&session_id, &server_ip, &out_ext);

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title: track.title,
            artist: track.artist_name,
            album: track.album_title,
            duration_ms: Some(track.duration_ms),
            source: "local".into(),
            cover_url: track.cover_path,
            stream_id: Some(session_id),
            file_size: resolved_file_size,
            sample_rate: resolved_sr,
            bit_depth: resolved_bd,
            channels: resolved_ch,
        })
    }

    async fn resolve_streaming_url(
        &self,
        service_name: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .as_deref()
            .ok_or("source_id required for streaming")?;

        // Check for prefetched PCM data before downloading.
        // If the prefetch engine has already decoded this track, serve
        // the PCM directly via a streaming session — zero download delay.
        // Skip prefetch for network outputs (DLNA) when buffer is truncated
        // (30s mode) — the renderer needs the full file.
        if let Some(prefetched) = self.prefetch.take_prefetched(service_name, source_id).await {
            let is_network = req
                .output_device_id
                .as_deref()
                .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
            let bytes_per_sec = (prefetched.sample_rate as usize)
                * (prefetched.bit_depth as usize / 8)
                * (prefetched.channels as usize);
            let buffered_ms = if bytes_per_sec > 0 {
                (prefetched.pcm_data.len() as u64 * 1000) / bytes_per_sec as u64
            } else {
                0
            };
            let is_truncated =
                prefetched.duration_ms > 0 && buffered_ms + 2000 < prefetched.duration_ms;

            if is_network && is_truncated {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    buffered_ms,
                    duration_ms = prefetched.duration_ms,
                    "prefetch_skip_truncated_for_dlna"
                );
            } else {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    title = %prefetched.title,
                    buffer_bytes = prefetched.pcm_data.len(),
                    "prefetch_hit_serving_buffered_pcm"
                );
                return self.serve_prefetched_pcm(prefetched, req).await;
            }
        }

        let registry = self.services.lock().await;
        let svc = registry
            .get(service_name)
            .ok_or_else(|| format!("unknown service: {service_name}"))?;
        let mut svc = svc.lock().await;

        // Try to get the track URL; if it fails with an auth error, attempt
        // a token refresh and retry once. This handles Qobuz tokens expiring
        // mid-session (search still works without auth, but playback doesn't).
        let stream_data = match svc.get_track_url(source_id, None).await {
            Ok(data) => data,
            Err(ref e)
                if {
                    let msg = e.to_string();
                    msg.contains("401") || msg.contains("403")
                } =>
            {
                info!(
                    service = service_name,
                    error = %e,
                    "streaming_auth_error_attempting_refresh"
                );
                if svc.refresh_if_needed().await.unwrap_or(false) {
                    svc.get_track_url(source_id, None)
                        .await
                        .map_err(|e| e.to_string())?
                } else {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        };

        let info = StreamInfo {
            format: stream_data.quality.codec.to_lowercase(),
            mime_type: stream_data.mime_type.clone(),
            sample_rate: stream_data.quality.sample_rate,
            bit_depth: stream_data.quality.bit_depth,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };

        let is_https = stream_data.url.starts_with("https://");
        // file:// URLs come from Tidal DASH multi-segment downloads — the fMP4
        // has already been assembled on disk by get_track_url().
        let is_dash_file = stream_data.url.starts_with("file://");
        let is_oaat_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));

        // Local and OAAT outputs expect raw PCM in a WAV container.
        // Streaming services deliver compressed audio (FLAC, AAC, etc.)
        // which LocalOutput cannot decode — it would interpret compressed
        // bytes as raw PCM samples, producing white noise.
        // Fix: download → decode → WAV transcode, same as local files.
        let (stream_url, sid, out_mime, stream_file_size) = if is_local_stream || is_oaat_stream {
            let upstream_url = stream_data.url.clone();
            let codec = stream_data.quality.codec.to_lowercase();
            let sr = stream_data.quality.sample_rate;
            // Local output: 32-bit to avoid 24-bit byte misalignment noise
            // (see local_needs_wav comment in resolve_local_track).
            // OAAT: cap at 24-bit (endpoints may not support 32-bit WAV).
            let bd = if is_local_stream {
                32
            } else {
                stream_data.quality.bit_depth.max(16).min(24)
            };

            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: sr,
                bit_depth: bd,
                channels: 2,
                file_size: None,
                duration_ms: None,
                ..Default::default()
            };

            // Guard against a stale/cleaned-up DASH temp file (mirrors the
            // `is_dash_file` DLNA path below). The local transcode runs
            // fire-and-forget in a spawned task, so a missing file would decode
            // to nothing while play() still reports output_sent=true. Fail early
            // so the caller sees the real failure instead of silent no-playback.
            // (Reported on ASIO with 24/192 Tidal DASH after the temp file is gone.)
            if upstream_url.starts_with("file://") {
                let fp = upstream_url
                    .strip_prefix("file://")
                    .unwrap_or(&upstream_url);
                let size = std::fs::metadata(fp).map(|m| m.len()).unwrap_or(0);
                if size == 0 {
                    warn!(path = %fp, "streaming_dash_file_missing_or_empty");
                    return Err(format!(
                        "DASH temp file missing or empty (needs re-download): {fp}"
                    ));
                }
            }

            let (session_id, tx, data_ready) =
                self.streamer.create_session(wav_info, false, 256).await;

            {
                let sessions = self.streamer.sessions_state();
                let sessions = sessions.lock().await;
                if let Some(session) = sessions.get(&session_id) {
                    session
                        .wav_header_included
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }

            info!(
                service = service_name,
                codec = %codec,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_transcode_to_wav_for_local_output"
            );

            let ev_bus = self.event_bus.clone();
            let zone_id = req.zone_id;

            // Detect file:// URLs from DASH multi-segment downloads — the fMP4
            // is already on disk, skip the HTTP download step.
            let is_dash_local = upstream_url.starts_with("file://");

            // Background task: download upstream → temp file → decode → WAV → session
            tokio::spawn(async move {
                // Audio-levels channel so the web client VU-meter works for
                // streaming-service content played through local/OAAT outputs.
                let (levels_tx, mut levels_rx) =
                    tokio::sync::mpsc::unbounded_channel::<crate::audio::levels::AudioLevels>();
                if let Some(ref bus) = ev_bus {
                    let bus = bus.clone();
                    tokio::spawn(async move {
                        while let Some(lvl) = levels_rx.recv().await {
                            bus.emit(
                                "playback.audio_levels",
                                serde_json::json!({
                                    "zone_id": zone_id,
                                    "rms_left_db": lvl.rms_left_db(),
                                    "rms_right_db": lvl.rms_right_db(),
                                    "peak_left_db": lvl.peak_left_db(),
                                    "peak_right_db": lvl.peak_right_db(),
                                    "rms_left": lvl.rms_left,
                                    "rms_right": lvl.rms_right,
                                    "spectrum": lvl.spectrum,
                                }),
                            );
                        }
                    });
                }

                // For DASH file:// URLs the fMP4 is already on disk — use it
                // directly instead of downloading via HTTP.
                let tmp_file = if is_dash_local {
                    let file_path = upstream_url
                        .strip_prefix("file://")
                        .unwrap_or(&upstream_url)
                        .to_string();
                    let file_size = std::fs::metadata(&file_path)
                        .ok()
                        .map(|m| m.len())
                        .unwrap_or(0);
                    info!(
                        path = %file_path,
                        file_size,
                        "streaming_dash_file_already_on_disk"
                    );
                    file_path
                } else {
                    // Download to temp file on a blocking thread
                    let tmp_path = std::env::temp_dir()
                        .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                        .to_string_lossy()
                        .to_string();
                    let tmp_path_clone = tmp_path.clone();
                    let upstream = upstream_url.clone();
                    let download_result = tokio::task::spawn_blocking(move || {
                        let resp = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream).send());
                        match resp {
                            Ok(mut r) if r.status().is_success() => {
                                let mut file = match std::fs::File::create(&tmp_path_clone) {
                                    Ok(f) => f,
                                    Err(e) => return Err(format!("tmp create: {e}")),
                                };
                                match std::io::copy(&mut r, &mut file) {
                                    Ok(bytes) => {
                                        debug!(bytes, path = %tmp_path_clone, "streaming_download_complete");
                                        Ok(tmp_path_clone)
                                    }
                                    Err(e) => Err(format!("download copy: {e}")),
                                }
                            }
                            Ok(r) => Err(format!("upstream HTTP {}", r.status())),
                            Err(e) => Err(format!("upstream fetch: {e}")),
                        }
                    })
                    .await;

                    match download_result {
                        Ok(Ok(path)) => path,
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_transcode_download_failed");
                            let _ = std::fs::remove_file(&tmp_path);
                            return;
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_transcode_task_join_failed");
                            let _ = std::fs::remove_file(&tmp_path);
                            return;
                        }
                    }
                };

                // Progressive decode: stream PCM chunks as they are decoded.
                // The DLNA renderer can start fetching as soon as first chunks
                // arrive, reducing transition latency after download completes.
                let tmp_file_clone = tmp_file.clone();
                let tx_clone = tx.clone();
                // Drop the original sender so the channel closes when decode finishes.
                drop(tx);
                let decode_result = tokio::task::spawn_blocking(move || {
                    crate::audio::decode::decode_to_pcm_streaming_with_levels(
                        &tmp_file_clone,
                        Some(sr),
                        Some(2),
                        Some(bd),
                        tx_clone,
                        32768,
                        data_ready,
                        levels_tx,
                    )
                })
                .await;

                // Clean up temp file
                let _ = std::fs::remove_file(&tmp_file);

                match decode_result {
                    Ok(Ok((_bit_depth, actual_rate))) => {
                        if actual_rate != sr {
                            tracing::info!(
                                api_rate = sr,
                                actual_rate,
                                "streaming_sample_rate_mismatch_wav_header_has_correct_rate"
                            );
                        }
                        debug!("streaming_transcode_complete_progressive");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_transcode_decode_failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_transcode_decode_task_panic");
                    }
                }
            });

            let server_ip = self.server_ip();
            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (url, Some(session_id), "audio/wav".to_string(), None)
        } else if is_dash_file {
            // DASH multi-segment fMP4 already assembled on disk by get_track_url().
            // DLNA renderers can't decode fMP4+FLAC directly, and chunked WAV
            // causes noise on many renderers (darTZeel, Eversolo, etc.).
            // Pre-transcode to a FLAC temp file so we can serve with Content-Length.
            let dash_file_path = stream_data
                .url
                .strip_prefix("file://")
                .unwrap_or(&stream_data.url)
                .to_string();

            if !std::path::Path::new(&dash_file_path).exists() {
                warn!(path = %dash_file_path, "streaming_dash_file_missing_skipping_decode");
                return Err("DASH file missing (already consumed by prior decode)".into());
            }

            let unique_path = format!("{}.decoding", &dash_file_path);
            if std::fs::rename(&dash_file_path, &unique_path).is_err() {
                warn!(path = %dash_file_path, "streaming_dash_file_already_being_decoded");
                return Err("DASH file already being decoded".into());
            }

            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);

            let tmp_path = std::env::temp_dir()
                .join(format!("tune-dash-transcode-{}.flac", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .to_string();

            info!(
                path = %unique_path,
                tmp = %tmp_path,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_dash_pre_transcode_to_flac"
            );

            let tmp_path_clone = tmp_path.clone();
            let unique_path_clone = unique_path.clone();
            let eq_profile_pretranscode = self.load_eq_processor(req.zone_id, sr, 2);
            let transcode_result = tokio::task::spawn_blocking(move || {
                let decoded = crate::audio::decode::decode_to_pcm(
                    &unique_path_clone,
                    Some(sr),
                    Some(2),
                    0.0,
                    0.0,
                )?;

                let mut pcm_bytes = decoded.pcm_bytes();
                let actual_bd = decoded.bit_depth;

                if let Some(mut eq) = eq_profile_pretranscode {
                    eq.process_pcm(&mut pcm_bytes, actual_bd);
                }

                let rt = tokio::runtime::Handle::try_current()
                    .map_err(|e| format!("no tokio runtime: {e}"))?;
                let encoded_data = rt.block_on(async {
                    let mut encoder = crate::audio::encoder::AudioEncoder::new(
                        "flac",
                        decoded.sample_rate,
                        actual_bd as u32,
                        decoded.channels,
                    );
                    encoder.start().await?;
                    encoder.write(&pcm_bytes).await?;
                    encoder.finish().await
                })?;

                std::fs::write(&tmp_path_clone, &encoded_data)
                    .map_err(|e| format!("write temp file: {e}"))?;

                let file_size = encoded_data.len() as u64;
                Ok::<(u64, u16), String>((file_size, actual_bd))
            })
            .await;

            let _ = std::fs::remove_file(&unique_path);

            match transcode_result {
                Ok(Ok((file_size, actual_bd))) => {
                    info!(
                        tmp = %tmp_path,
                        file_size,
                        bit_depth = actual_bd,
                        "streaming_dash_pre_transcode_complete"
                    );

                    let file_info = StreamInfo {
                        format: "flac".into(),
                        mime_type: "audio/flac".into(),
                        sample_rate: sr,
                        bit_depth: bd,
                        channels: 2,
                        file_size: Some(file_size),
                        duration_ms: None,
                        ..Default::default()
                    };
                    let session_id = self
                        .streamer
                        .create_file_session(file_info, tmp_path, false)
                        .await;

                    let server_ip = self.server_ip();
                    let url = self
                        .streamer
                        .get_stream_url(&session_id, &server_ip, "flac");
                    (
                        url,
                        Some(session_id),
                        "audio/flac".to_string(),
                        Some(file_size),
                    )
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_failed");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode failed: {e}"));
                }
                Err(e) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_task_panic");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode task panic: {e}"));
                }
            }
        } else if is_https {
            let codec_lower = stream_data.quality.codec.to_lowercase();
            // Codecs that legacy DLNA renderers can't decode must be
            // pre-transcoded to FLAC. AAC/MP4 (most renderers reject AAC over
            // DLNA) plus Opus/Ogg-Vorbis: YouTube delivers Opus-in-WebM, which
            // old renderers like the Cyrus Stream X reject outright (no
            // audio/webm or audio/opus sink), leaving the transport in
            // ERROR_OCCURRED.
            let needs_flac_transcode = codec_lower == "aac"
                || codec_lower == "mp4"
                || stream_data.mime_type.contains("mp4")
                || AudioFormat::from_extension(&codec_lower)
                    .is_some_and(|f| f.needs_transcode_for_dlna());

            if needs_flac_transcode {
                // AAC/MP4 streams need transcoding for DLNA — most renderers
                // (DMP-A8, etc.) don't support AAC via DLNA.  Pre-transcode to
                // FLAC temp file so we serve with Content-Length (chunked WAV
                // causes noise on many renderers).
                let sr = stream_data.quality.sample_rate;
                let bd = stream_data.quality.bit_depth.max(16).min(24);

                info!(
                    service = service_name,
                    codec = %codec_lower,
                    sample_rate = sr,
                    bit_depth = bd,
                    "streaming_aac_pre_transcode_to_wav_for_dlna"
                );

                let upstream_url = stream_data.url.clone();
                let codec = codec_lower.clone();
                let tmp_dl = std::env::temp_dir()
                    .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                    .to_string_lossy()
                    .to_string();
                let tmp_flac = std::env::temp_dir()
                    .join(format!("tune-aac-transcode-{}.wav", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .to_string();

                let tmp_dl_clone = tmp_dl.clone();
                let tmp_flac_clone = tmp_flac.clone();
                let transcode_result = tokio::task::spawn_blocking(move || {
                    // 1. Download
                    let resp = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(120))
                        .build()
                        .and_then(|c| c.get(&upstream_url).send())
                        .map_err(|e| format!("upstream fetch: {e}"))?;
                    if !resp.status().is_success() {
                        return Err(format!("upstream HTTP {}", resp.status()));
                    }
                    let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                    std::fs::write(&tmp_dl_clone, &bytes).map_err(|e| format!("write dl: {e}"))?;

                    // 2. Decode to PCM
                    let decoded = crate::audio::decode::decode_to_pcm(
                        &tmp_dl_clone,
                        Some(sr),
                        Some(2),
                        0.0,
                        0.0,
                    )?;
                    let pcm_bytes = decoded.pcm_bytes();
                    let actual_bd = decoded.bit_depth;

                    // 3. Encode to FLAC (lossless, with Content-Length)
                    let rt = tokio::runtime::Handle::try_current()
                        .map_err(|e| format!("no tokio runtime: {e}"))?;
                    let encoded_data = rt.block_on(async {
                        let mut encoder = crate::audio::encoder::AudioEncoder::new(
                            "flac",
                            decoded.sample_rate,
                            actual_bd as u32,
                            decoded.channels,
                        );
                        encoder.start().await?;
                        encoder.write(&pcm_bytes).await?;
                        encoder.finish().await
                    })?;

                    std::fs::write(&tmp_flac_clone, &encoded_data)
                        .map_err(|e| format!("write flac: {e}"))?;

                    let _ = std::fs::remove_file(&tmp_dl_clone);
                    let file_size = encoded_data.len() as u64;
                    Ok::<(u64, u16), String>((file_size, actual_bd))
                })
                .await;

                match transcode_result {
                    Ok(Ok((file_size, actual_bd))) => {
                        info!(
                            tmp = %tmp_flac,
                            file_size,
                            bit_depth = actual_bd,
                            "streaming_aac_pre_transcode_complete"
                        );

                        let file_info = StreamInfo {
                            format: "wav".into(),
                            mime_type: "audio/wav".into(),
                            sample_rate: sr,
                            bit_depth: bd,
                            channels: 2,
                            file_size: Some(file_size),
                            duration_ms: None,
                            ..Default::default()
                        };
                        let session_id = self
                            .streamer
                            .create_file_session(file_info, tmp_flac, false)
                            .await;

                        let server_ip = self.server_ip();
                        let url = self
                            .streamer
                            .get_stream_url(&session_id, &server_ip, "flac");
                        (
                            url,
                            Some(session_id),
                            "audio/flac".to_string(),
                            Some(file_size),
                        )
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_aac_pre_transcode_failed");
                        let _ = std::fs::remove_file(&tmp_dl);
                        let _ = std::fs::remove_file(&tmp_flac);
                        return Err(format!("AAC transcode failed: {e}"));
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_aac_pre_transcode_task_panic");
                        let _ = std::fs::remove_file(&tmp_dl);
                        let _ = std::fs::remove_file(&tmp_flac);
                        return Err(format!("AAC transcode task panic: {e}"));
                    }
                }
            } else {
                // Non-AAC codecs (FLAC, etc.) — check if the DLNA renderer
                // actually supports this MIME type before proxying directly.
                // Strict renderers (Denon, Marantz, Revox) reject FLAC because
                // their GetProtocolInfo Sink doesn't list audio/flac.  In that
                // case, transcode to WAV (LPCM) which has a proper DLNA.ORG_PN
                // profile and is universally supported.
                let zone = ZoneRepo::with_backend(self.db.clone())
                    .get(req.zone_id)
                    .ok()
                    .flatten();
                let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
                let is_dlna = zone_output_type.as_deref() == Some("dlna");
                let device_id = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let renderer_supports_mime = if is_dlna
                    && (stream_data.mime_type == "audio/flac"
                        || stream_data.mime_type == "audio/x-flac")
                    && !device_id.is_empty()
                {
                    self.dlna_supports_mime(device_id, &stream_data.mime_type)
                        .await
                } else {
                    true
                };

                if !renderer_supports_mime {
                    // Renderer does not support FLAC — transcode to WAV (LPCM).
                    // Same pattern as AAC pre-transcode: download → decode → encode → file session.
                    let sr = stream_data.quality.sample_rate;
                    let bd = stream_data.quality.bit_depth.max(16).min(24);

                    info!(
                        service = service_name,
                        codec = %codec_lower,
                        device = %device_id,
                        sample_rate = sr,
                        bit_depth = bd,
                        "streaming_flac_transcode_to_wav_renderer_unsupported"
                    );

                    let upstream_url = stream_data.url.clone();
                    let tmp_dl = std::env::temp_dir()
                        .join(format!(
                            "tune-stream-{}.{}",
                            uuid::Uuid::new_v4(),
                            codec_lower
                        ))
                        .to_string_lossy()
                        .to_string();
                    let tmp_wav = std::env::temp_dir()
                        .join(format!("tune-flac-to-wav-{}.wav", uuid::Uuid::new_v4()))
                        .to_string_lossy()
                        .to_string();

                    let tmp_dl_clone = tmp_dl.clone();
                    let tmp_wav_clone = tmp_wav.clone();
                    let transcode_result = tokio::task::spawn_blocking(move || {
                        // 1. Download
                        let resp = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream_url).send())
                            .map_err(|e| format!("upstream fetch: {e}"))?;
                        if !resp.status().is_success() {
                            return Err(format!("upstream HTTP {}", resp.status()));
                        }
                        let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                        std::fs::write(&tmp_dl_clone, &bytes)
                            .map_err(|e| format!("write dl: {e}"))?;

                        // 2. Decode to PCM
                        let decoded = crate::audio::decode::decode_to_pcm(
                            &tmp_dl_clone,
                            Some(sr),
                            Some(2),
                            0.0,
                            0.0,
                        )?;
                        let pcm_bytes = decoded.pcm_bytes();
                        let actual_bd = decoded.bit_depth;
                        let actual_sr = decoded.sample_rate;
                        let actual_ch = decoded.channels;

                        // 3. Encode to WAV
                        let rt = tokio::runtime::Handle::try_current()
                            .map_err(|e| format!("no tokio runtime: {e}"))?;
                        let encoded_data = rt.block_on(async {
                            let mut encoder = crate::audio::encoder::AudioEncoder::new(
                                "wav",
                                actual_sr,
                                actual_bd as u32,
                                actual_ch,
                            );
                            encoder.start().await?;
                            encoder.write(&pcm_bytes).await?;
                            encoder.finish().await
                        })?;

                        std::fs::write(&tmp_wav_clone, &encoded_data)
                            .map_err(|e| format!("write wav: {e}"))?;

                        let _ = std::fs::remove_file(&tmp_dl_clone);
                        let file_size = encoded_data.len() as u64;
                        Ok::<(u64, u16, u32, u16), String>((
                            file_size,
                            actual_bd,
                            actual_sr,
                            actual_ch as u16,
                        ))
                    })
                    .await;

                    match transcode_result {
                        Ok(Ok((file_size, actual_bd, actual_sr, actual_ch))) => {
                            info!(
                                tmp = %tmp_wav,
                                file_size,
                                bit_depth = actual_bd,
                                sample_rate = actual_sr,
                                "streaming_flac_to_wav_transcode_complete"
                            );

                            let file_info = StreamInfo {
                                format: "wav".into(),
                                mime_type: "audio/wav".into(),
                                sample_rate: actual_sr,
                                bit_depth: actual_bd,
                                channels: actual_ch,
                                file_size: Some(file_size),
                                duration_ms: None,
                                ..Default::default()
                            };
                            let session_id = self
                                .streamer
                                .create_file_session(file_info, tmp_wav, false)
                                .await;

                            let server_ip = self.server_ip();
                            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                            (
                                url,
                                Some(session_id),
                                "audio/wav".to_string(),
                                Some(file_size),
                            )
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_flac_to_wav_transcode_failed");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("FLAC→WAV transcode failed: {e}"));
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_flac_to_wav_transcode_task_panic");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("FLAC→WAV transcode task panic: {e}"));
                        }
                    }
                } else {
                    // Renderer supports FLAC — proxy directly as before
                    let session_id = self
                        .streamer
                        .create_proxy_session(info, stream_data.url.clone(), false)
                        .await;
                    let server_ip = self.server_ip();
                    let url = self
                        .streamer
                        .get_stream_url(&session_id, &server_ip, &codec_lower);
                    (url, Some(session_id), stream_data.mime_type.clone(), None)
                }
            }
        } else {
            (
                stream_data.url.clone(),
                None,
                stream_data.mime_type.clone(),
                None,
            )
        };

        let (title, artist, album, duration_ms, cover_path) = if req.title.is_some() {
            (
                req.title.clone().unwrap_or_default(),
                req.artist_name.clone(),
                req.album_title.clone(),
                req.duration_ms,
                req.cover_url.clone(),
            )
        } else {
            match svc.get_track(source_id).await {
                Ok(track) => (
                    track.title,
                    Some(track.artist),
                    track.album,
                    Some(track.duration_ms as i64),
                    track.cover_path,
                ),
                Err(_) => ("Unknown".into(), None, None, req.duration_ms, None),
            }
        };

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source: service_name.into(),
            cover_url: cover_path,
            stream_id: sid,
            file_size: stream_file_size,
            sample_rate: Some(stream_data.quality.sample_rate),
            bit_depth: Some(stream_data.quality.bit_depth as u32),
            channels: Some(2),
        })
    }

    /// Serve prefetched PCM data as a WAV stream session.
    ///
    /// Creates a streaming session and feeds the already-decoded PCM into it,
    /// bypassing the download+decode pipeline entirely.
    async fn serve_prefetched_pcm(
        &self,
        prefetched: crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let sr = prefetched.sample_rate;
        let bd = prefetched.bit_depth;
        let ch = prefetched.channels;

        // Prefer the request's metadata (from now_playing) over the prefetch
        // buffer's. The buffer is built for the *next* track and can carry an
        // empty title (prefetched before its metadata was resolved); serving it
        // verbatim after a seek wipes the Now Playing title (DEvir: title
        // disappears when seeking shortly after a TIDAL track starts).
        let title = req
            .title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| prefetched.title.clone());
        let artist = req
            .artist_name
            .clone()
            .or_else(|| prefetched.artist.clone());
        let album = req.album_title.clone().or_else(|| prefetched.album.clone());
        let cover_url = req
            .cover_url
            .clone()
            .or_else(|| prefetched.cover_url.clone());

        // Determine output bit depth based on output type
        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_network_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
        let out_bd = if is_local_stream {
            32
        } else {
            bd.max(16).min(24)
        };

        // For DLNA/network outputs, encode prefetched PCM to a file.
        // Use FLAC if the renderer supports it, otherwise WAV.
        if is_network_output {
            let use_wav = if let Some(device_id) = req.output_device_id.as_deref() {
                !self.dlna_supports_mime(device_id, "audio/flac").await
            } else {
                false
            };
            let ext = if use_wav { "wav" } else { "flac" };
            let tmp_path =
                std::env::temp_dir().join(format!("tune-prefetch-{}.{ext}", uuid::Uuid::new_v4()));
            let tmp_str = tmp_path.to_string_lossy().to_string();
            let pcm_data = prefetched.pcm_data;
            let encode_sr = sr;
            let encode_bd = out_bd;
            let encode_ch = ch;
            let encode_path = tmp_str.clone();
            let encode_wav = use_wav;
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let data_size = pcm_data.len() as u32;
                let byte_rate = encode_sr * encode_ch as u32 * (encode_bd as u32 / 8);
                let block_align = encode_ch as u16 * (encode_bd as u16 / 8);
                if encode_wav {
                    let mut f = std::fs::File::create(&encode_path)
                        .map_err(|e| format!("create tmp wav: {e}"))?;
                    let mut hdr = Vec::with_capacity(44);
                    hdr.extend_from_slice(b"RIFF");
                    hdr.extend_from_slice(&(36 + data_size).to_le_bytes());
                    hdr.extend_from_slice(b"WAVEfmt ");
                    hdr.extend_from_slice(&16u32.to_le_bytes());
                    hdr.extend_from_slice(&1u16.to_le_bytes());
                    hdr.extend_from_slice(&(encode_ch as u16).to_le_bytes());
                    hdr.extend_from_slice(&encode_sr.to_le_bytes());
                    hdr.extend_from_slice(&byte_rate.to_le_bytes());
                    hdr.extend_from_slice(&block_align.to_le_bytes());
                    hdr.extend_from_slice(&(encode_bd as u16).to_le_bytes());
                    hdr.extend_from_slice(b"data");
                    hdr.extend_from_slice(&data_size.to_le_bytes());
                    f.write_all(&hdr)
                        .map_err(|e| format!("write wav header: {e}"))?;
                    f.write_all(&pcm_data)
                        .map_err(|e| format!("write wav pcm: {e}"))?;
                    Ok(())
                } else {
                    let tmp_wav = format!("{}.wav", encode_path);
                    {
                        let mut f = std::fs::File::create(&tmp_wav)
                            .map_err(|e| format!("create tmp wav: {e}"))?;
                        let mut hdr = Vec::with_capacity(44);
                        hdr.extend_from_slice(b"RIFF");
                        hdr.extend_from_slice(&(36 + data_size).to_le_bytes());
                        hdr.extend_from_slice(b"WAVEfmt ");
                        hdr.extend_from_slice(&16u32.to_le_bytes());
                        hdr.extend_from_slice(&1u16.to_le_bytes());
                        hdr.extend_from_slice(&(encode_ch as u16).to_le_bytes());
                        hdr.extend_from_slice(&encode_sr.to_le_bytes());
                        hdr.extend_from_slice(&byte_rate.to_le_bytes());
                        hdr.extend_from_slice(&block_align.to_le_bytes());
                        hdr.extend_from_slice(&(encode_bd as u16).to_le_bytes());
                        hdr.extend_from_slice(b"data");
                        hdr.extend_from_slice(&data_size.to_le_bytes());
                        f.write_all(&hdr)
                            .map_err(|e| format!("write wav header: {e}"))?;
                        f.write_all(&pcm_data)
                            .map_err(|e| format!("write wav pcm: {e}"))?;
                    }
                    let status = std::process::Command::new("ffmpeg")
                        .args(["-y", "-i", &tmp_wav, "-c:a", "flac", &encode_path])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let _ = std::fs::remove_file(&tmp_wav);
                    match status {
                        Ok(s) if s.success() => Ok(()),
                        Ok(s) => Err(format!("ffmpeg exit {s}")),
                        Err(e) => Err(format!("ffmpeg: {e}")),
                    }
                }
            })
            .await
            .map_err(|e| format!("spawn: {e}"))??;

            let file_size = std::fs::metadata(&tmp_str).map(|m| m.len()).unwrap_or(0);
            let (out_format, out_mime) = if use_wav {
                ("wav", "audio/wav")
            } else {
                ("flac", "audio/flac")
            };
            info!(
                title = %prefetched.title,
                file_size,
                format = out_format,
                "prefetch_pcm_encoded_for_dlna"
            );

            let flac_info = StreamInfo {
                format: out_format.into(),
                mime_type: out_mime.into(),
                sample_rate: sr,
                bit_depth: out_bd,
                channels: ch,
                file_size: Some(file_size),
                duration_ms: Some(prefetched.duration_ms),
                ..Default::default()
            };

            let session_id = self
                .streamer
                .create_file_session(flac_info, tmp_str.clone(), false)
                .await;

            let server_ip = self.server_ip();
            let stream_url = self
                .streamer
                .get_stream_url(&session_id, &server_ip, "flac");

            return Ok(ResolvedStream {
                url: stream_url,
                stream_id: Some(session_id),
                title: title.clone(),
                artist: artist.clone(),
                album: None,
                duration_ms: Some(prefetched.duration_ms as i64),
                source: prefetched.source,
                mime_type: "audio/flac".into(),
                sample_rate: Some(sr),
                bit_depth: Some(out_bd as u32),
                channels: Some(ch as u32),
                cover_url: cover_url.clone(),
                file_size: Some(file_size),
            });
        }

        let wav_info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: sr,
            bit_depth: out_bd,
            channels: ch,
            file_size: None,
            duration_ms: Some(prefetched.duration_ms),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(wav_info, false, 256).await;

        // Feed the prefetched PCM data into the session in chunks.
        // This happens nearly instantly since the data is already in memory.
        let pcm_data = prefetched.pcm_data;
        tokio::spawn(async move {
            let chunk_size = 32768;
            let mut first = true;
            for chunk in pcm_data.chunks(chunk_size) {
                if tx.send(chunk.to_vec()).await.is_err() {
                    debug!("prefetch_session_consumer_dropped");
                    return;
                }
                if first {
                    first = false;
                    data_ready.notify_one();
                }
            }
            if first {
                // No data was sent (empty buffer)
                data_ready.notify_one();
            }
            debug!("prefetch_pcm_feed_complete");
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: "audio/wav".into(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            duration_ms: Some(prefetched.duration_ms as i64),
            source: prefetched.source,
            cover_url: cover_url.clone(),
            stream_id: Some(session_id),
            file_size: None,
            sample_rate: Some(sr),
            bit_depth: Some(out_bd as u32),
            channels: Some(ch as u32),
        })
    }

    /// Convert a cover_path (which may be a short hash or a full URL) into an
    /// absolute HTTP URL accessible by network renderers (DLNA/OpenHome).
    /// Hash-only values like `"abc123def"` become `http://IP:PORT/api/v1/artwork/abc123def`.
    /// Full URLs (starting with `http://` or `https://`) are passed through unchanged.
    fn resolve_cover_url(&self, cover: Option<&str>) -> Option<String> {
        let c = cover?;
        if c.starts_with("http://") || c.starts_with("https://") {
            return Some(c.to_string());
        }
        // It's a local artwork hash — build an absolute URL
        let server_ip = self.server_ip();
        // Use the streamer port (same as API server port)
        let port = std::env::var("TUNE_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8888);
        Some(format!(
            "http://{server_ip}:{port}/api/v1/library/artwork/{c}"
        ))
    }

    /// Recreate a local (cpal) output on demand and play to it. Only the
    /// `local-audio` build has `outputs::local`; without that feature there is
    /// no local backend, so this is a no-op that reports the device as missing.
    #[cfg(feature = "local-audio")]
    async fn recreate_local_and_play(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        let device_name = device_id.strip_prefix("local:").unwrap_or(device_id);
        info!(device_id, "output_not_found_recreating_local_output");
        let local_out = crate::outputs::local::LocalOutput::new(device_name.to_string());
        if let Some(position_ms) = start_position_ms {
            local_out.set_pending_start_position_ms(position_ms);
            let producer_seeked = media.file_path.is_some();
            local_out.set_producer_seeked(producer_seeked);
        }
        {
            let mut outputs = self.outputs.lock().await;
            outputs.register(Box::new(local_out));
        }
        let outputs = self.outputs.lock().await;
        if let Some(arc) = outputs.get(device_id) {
            let output = arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id, "output_play_sent_after_recreate");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id, error = %e, "output_play_failed_after_recreate");
                    (false, Some(format!("Output device error: {e}")))
                }
            }
        } else {
            (false, Some(format!("Device not found: {device_id}")))
        }
    }

    #[cfg(not(feature = "local-audio"))]
    async fn recreate_local_and_play(
        &self,
        device_id: &str,
        _media: &crate::outputs::traits::PlayMedia<'_>,
        _start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        (false, Some(format!("Device not found: {device_id}")))
    }

    async fn send_to_output(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        let lock_start = std::time::Instant::now();
        let (output_arc, used_device_id) = {
            let outputs = self.outputs.lock().await;
            let elapsed = lock_start.elapsed();
            if elapsed.as_millis() > 200 {
                warn!(
                    device_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "send_to_output_lock_contention"
                );
            }
            // Bug 2 fix: never fall back to another zone/device.
            // If the exact requested device is not found, return an error so
            // audio never comes out of an unexpected speaker.
            match outputs.get(device_id) {
                Some(arc) => (Some(arc), device_id.to_string()),
                None => (None, device_id.to_string()),
            }
        };
        if let Some(output_arc) = output_arc {
            // For local outputs, set the pending start position before play
            #[cfg(feature = "local-audio")]
            if let Some(position_ms) = start_position_ms {
                if device_id.starts_with("local:") {
                    let output = output_arc.lock().await;
                    if let Some(local_output) = output
                        .as_any()
                        .downcast_ref::<crate::outputs::local::LocalOutput>()
                    {
                        local_output.set_pending_start_position_ms(position_ms);
                        // Only mark as pre-seeked when the media has a local
                        // file_path — meaning the decoder used seek_s. For
                        // streaming sources (TIDAL/Qobuz), the producer always
                        // starts from 0s and needs consumer-side skip.
                        let producer_seeked = media.file_path.is_some();
                        local_output.set_producer_seeked(producer_seeked);
                    }
                    drop(output);
                }
            }
            let output = output_arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id = %used_device_id, "output_play_sent");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id = %used_device_id, error = %e, "output_play_failed");
                    (false, Some(format!("Output device error: {e}")))
                }
            }
        } else if device_id.starts_with("local:") {
            self.recreate_local_and_play(device_id, media, start_position_ms)
                .await
        } else {
            warn!(device_id, "output_not_found");
            (
                false,
                Some(format!(
                    "Device not yet discovered: {device_id}. Please retry in a few seconds."
                )),
            )
        }
    }

    fn load_eq_processor(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::audio::eq::EqProcessor> {
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let key = format!("zone_{zone_id}_eq_profile");
        let profile: crate::audio::eq::EqProfile = settings
            .get(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !profile.enabled {
            return None;
        }
        let eq = crate::audio::eq::EqProcessor::new(&profile, sample_rate, channels);
        if eq.is_enabled() { Some(eq) } else { None }
    }

    fn record_listen(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        source: &str,
        source_id: Option<&str>,
        album_id: Option<i64>,
        duration_ms: i64,
        zone_id: i64,
        cover_url: Option<&str>,
    ) {
        // Resolve active profile from settings (null = default profile).
        let active_profile_id: Option<i64> = SettingsRepo::with_backend(self.db.clone())
            .get("active_profile_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());

        let repo = HistoryRepo::with_backend(self.db.clone());
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: title.into(),
            artist_name: artist.map(Into::into),
            album_title: album.map(Into::into),
            source: source.into(),
            source_id: source_id.map(Into::into),
            album_id,
            duration_ms,
            listened_at: None,
            zone_id: Some(zone_id),
            cover_url: cover_url.map(Into::into),
            profile_id: active_profile_id,
        })
        .ok();

        // Multi-service scrobble dispatch with tier gating.
        // Free tier: only the first configured service fires.
        // Premium tier: all configured services fire simultaneously.
        self.dispatch_scrobble(title, artist, album);
    }

    /// Dispatch scrobbles to all configured services, respecting tier limits.
    /// Free = 1 service max, Premium = all simultaneously.
    fn dispatch_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        // Check tier: if both services are active and user is Free, only
        // dispatch to the first one (Last.fm has priority as legacy default).
        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_scrobble(title, artist);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                // Either Last.fm is not active (so LB is the sole service)
                // or user is Premium (simultaneous allowed).
                self.listenbrainz_scrobble(title, artist, album);
            } else {
                debug!(
                    "listenbrainz_scrobble_skipped_free_tier: lastfm active, upgrade to Premium for multi-service"
                );
            }
        }
    }

    /// Dispatch now-playing updates to all configured services, respecting tier limits.
    fn dispatch_now_playing(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_now_playing(title, artist);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                self.listenbrainz_now_playing(title, artist, album);
            }
        }
    }

    fn lastfm_keys(&self) -> Option<(String, String, String)> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let api_key = settings.get("lastfm_api_key").ok().flatten()?;
        let api_secret = settings.get("lastfm_api_secret").ok().flatten()?;
        let session_key = settings.get("lastfm_session_key").ok().flatten()?;
        if api_key.is_empty() || api_secret.is_empty() || session_key.is_empty() {
            return None;
        }
        Some((api_key, api_secret, session_key))
    }

    fn lastfm_scrobble(&self, title: &str, artist: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::scrobble(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
                timestamp,
            )
            .await
            {
                warn!("lastfm_scrobble_error: {e}");
            }
        });
    }

    fn lastfm_now_playing(&self, title: &str, artist: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::update_now_playing(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
            )
            .await
            {
                warn!("lastfm_now_playing_error: {e}");
            }
        });
    }

    fn listenbrainz_token(&self) -> Option<String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings
            .get("listenbrainz_token")
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
    }

    fn listenbrainz_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let payload = serde_json::json!({
                "listen_type": "single",
                "payload": [{
                    "listened_at": timestamp,
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_scrobble_error: {e}");
            }
        });
    }

    fn listenbrainz_now_playing(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "listen_type": "playing_now",
                "payload": [{
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_now_playing_error: {e}");
            }
        });
    }

    pub async fn pause(&self, zone_id: i64, device_id: Option<&str>) {
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "paused")
            .ok();
        self.playback.pause(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.pause().await {
                    warn!(zone_id, error = %e, "device_pause_failed");
                }
            }
        }
    }

    pub async fn resume(&self, zone_id: i64, device_id: Option<&str>) {
        // Position is preserved across pause (playback state isn't reset), so we
        // know where to resume from.
        let position_ms = self.playback.get_state(zone_id).await.position_ms.max(0) as u64;
        self.playback.resume(zone_id).await;

        let Some(did) = device_id else { return };
        let output_type = {
            let outputs = self.outputs.lock().await;
            let Some(output) = outputs.get(did) else {
                return;
            };
            let out = output.lock().await;
            let t = out.output_type().to_string();
            if let Err(e) = out.resume().await {
                warn!(zone_id, error = %e, "device_resume_failed");
            }
            t
        };

        // Legacy DLNA/OpenHome renderers (e.g. Cyrus Stream X) restart the stream
        // on Play-after-Pause instead of resuming. Seek back to the paused
        // position once the renderer has had a moment to (re)start, so playback
        // continues instead of replaying from the top. Locks are released during
        // the wait so other zones aren't blocked.
        if (output_type == "dlna" || output_type == "openhome") && position_ms > 3000 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                match output.lock().await.seek(position_ms).await {
                    Ok(()) => info!(zone_id, position_ms, "dlna_resume_seek"),
                    Err(e) => warn!(zone_id, position_ms, error = %e, "dlna_resume_seek_failed"),
                }
            }
        }
    }

    pub async fn stop(&self, zone_id: i64, device_id: Option<&str>) {
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "stopped")
            .ok();
        self.cleanup_gapless_session(zone_id).await;
        self.prefetch.clear().await;
        let state = self.playback.get_state(zone_id).await;
        let old_stream_id = state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());
        self.playback.stop(zone_id).await;

        // Resolve device_id: prefer explicit, fall back to zone DB
        let resolved_did = match device_id {
            Some(d) => Some(d.to_string()),
            None => crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id),
        };
        if let Some(ref did) = resolved_did {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.stop().await {
                    warn!(zone_id, error = %e, "device_stop_failed");
                }
            }
        } else {
            // No device_id found — stop ALL registered outputs as fallback
            let outputs = self.outputs.lock().await;
            for did in outputs.list() {
                if let Some(output) = outputs.get(&did) {
                    let _ = output.lock().await.stop().await;
                }
            }
            warn!(zone_id, "stop_fallback_all_outputs_no_device_id");
        }
        // Remove session AFTER the output has been stopped
        if let Some(ref sid) = old_stream_id {
            self.streamer.remove_session(sid).await;
        }
    }

    pub async fn seek(&self, zone_id: i64, mut position_ms: u64, device_id: Option<&str>) {
        let seek_start = std::time::Instant::now();
        // Clamp seek to track duration to prevent out-of-bounds seek on files
        // with incorrect metadata duration (e.g. VBR MP3 with wrong header).
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            if np.duration_ms > 0 && position_ms > np.duration_ms as u64 {
                info!(
                    zone_id,
                    requested = position_ms,
                    duration = np.duration_ms,
                    "seek_clamped_to_duration"
                );
                position_ms = (np.duration_ms as u64).saturating_sub(1000);
            }
        }
        self.playback.seek(zone_id, position_ms as i64).await;
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            if let Err(e) = ZoneRepo::with_backend(self.db.clone()).save_playback_position(
                zone_id,
                position_ms as i64,
                np.track_id,
                Some(np.source.as_str()),
                np.source_id.as_deref(),
            ) {
                warn!(zone_id, error = %e, "persist_seek_position_failed");
            }
        }

        if let Some(did) = device_id {
            // For streaming tracks on network outputs (DLNA, OpenHome, etc.),
            // the seek strategy depends on whether the stream session supports
            // HTTP Range-based seeking:
            //
            // - Proxy sessions (FLAC from Tidal/Qobuz CDN) and file sessions
            //   support Range requests.  The renderer can seek by closing the
            //   current HTTP connection and re-requesting with a byte offset.
            //   For these, a direct SOAP Seek command is sufficient — the
            //   renderer handles the rest.
            //
            // - Decoded/transcoded sessions (WAV via mpsc channel) do NOT
            //   support Range seeking.  For these, we must recreate the stream
            //   session as a fallback.
            let is_streaming_source = state
                .now_playing
                .as_ref()
                .map(|np| {
                    np.source != "local"
                        && np.source != "radio"
                        && np.source != "podcast"
                        && np.stream_id.is_some()
                })
                .unwrap_or(false);

            // Determine output type from zone DB (avoids locking the output)
            let zone_output_type = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_type);
            let is_network = matches!(
                zone_output_type.as_deref(),
                Some("dlna")
                    | Some("openhome")
                    | Some("chromecast")
                    | Some("bluos")
                    | Some("squeezebox")
            );

            if is_streaming_source && is_network {
                // Check if the current stream session supports Range seeking
                let stream_id = state
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.clone());
                let is_seekable = if let Some(ref sid) = stream_id {
                    self.streamer.is_seekable_session(sid).await
                } else {
                    false
                };

                if is_seekable {
                    // Proxy/file session: the stream handler already supports
                    // Range-based seeking.  Send a direct SOAP Seek — the
                    // renderer will close the current connection and re-request
                    // with the appropriate byte offset.  Same stream URL, no
                    // interruption, no "new track" artifact.
                    info!(
                        zone_id,
                        position_ms,
                        source = ?state.now_playing.as_ref().map(|np| &np.source),
                        stream_id = ?stream_id,
                        "seek_streaming_direct_on_seekable_session"
                    );

                    let outputs = self.outputs.lock().await;
                    if let Some(output) = outputs.get(did) {
                        if let Err(e) = output.lock().await.seek(position_ms).await {
                            warn!(zone_id, error = %e, "device_seek_on_seekable_session_failed");
                        }
                    }
                    self.playback.seek(zone_id, position_ms as i64).await;
                    info!(
                        zone_id,
                        position_ms,
                        seek_ms = seek_start.elapsed().as_millis() as u64,
                        "seek_streaming_direct_complete"
                    );
                } else {
                    // Decoded/transcoded session (WAV via mpsc): no Range
                    // support.  Recreate the stream so the renderer gets a
                    // fresh URL to buffer from.
                    info!(
                        zone_id,
                        position_ms,
                        source = ?state.now_playing.as_ref().map(|np| &np.source),
                        "seek_streaming_on_network_output_recreating_stream"
                    );

                    // Pre-set the seek timestamp BEFORE play() so the poller's
                    // seek grace period covers the entire stream-recreation
                    // window.  play() calls playback.play() which increments
                    // track_generation and clears last_seek_at — we re-set it
                    // again after play() returns (and once more after the Seek
                    // command) to maintain continuous coverage.
                    self.playback.seek(zone_id, position_ms as i64).await;

                    // Re-create the stream: build a PlayRequest from the current NowPlaying
                    let np = state.now_playing.as_ref().unwrap();
                    let output_device_id = ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id);
                    let req = PlayRequest {
                        zone_id,
                        output_device_id,
                        track_id: np.track_id,
                        source: Some(np.source.clone()),
                        source_id: np.source_id.clone(),
                        title: Some(np.title.clone()),
                        artist_name: np.artist_name.clone(),
                        album_title: np.album_title.clone(),
                        cover_url: np.cover_path.clone(),
                        duration_ms: Some(np.duration_ms),
                        seek_ms: None,
                        temp_file_path: None,
                    };

                    match self.play(req).await {
                        Ok(_) => {
                            // play() cleared last_seek_at — re-set it immediately
                            // so the poller's seek grace covers the buffering window.
                            self.playback.seek(zone_id, position_ms as i64).await;

                            // Stream is now fresh — issue the seek on the output.
                            // Small delay to let the renderer start buffering.
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let outputs = self.outputs.lock().await;
                            if let Some(output) = outputs.get(did) {
                                if let Err(e) = output.lock().await.seek(position_ms).await {
                                    warn!(zone_id, error = %e, "device_seek_after_stream_recreate_failed");
                                }
                            }
                            // Re-set the seek timestamp so the poller grace period
                            // starts from after the Seek SOAP command, not from
                            // the play() call.
                            self.playback.seek(zone_id, position_ms as i64).await;
                            info!(
                                zone_id,
                                position_ms,
                                seek_ms = seek_start.elapsed().as_millis() as u64,
                                "seek_streaming_complete"
                            );
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "seek_streaming_play_recreate_failed");
                            // Restore seek timestamp so the poller doesn't
                            // misinterpret the Stopped state as a playback failure.
                            self.playback.seek(zone_id, position_ms as i64).await;
                            // Fall back to direct seek (best effort)
                            let outputs = self.outputs.lock().await;
                            if let Some(output) = outputs.get(did) {
                                if let Err(e) = output.lock().await.seek(position_ms).await {
                                    warn!(zone_id, error = %e, "device_seek_fallback_failed");
                                }
                            }
                        }
                    }
                }
            } else {
                // Local output: the WAV stream is sequential (mpsc channel),
                // so we must stop+replay from the seek position.
                let is_local_output =
                    zone_output_type.as_deref() == Some("local") || zone_output_type.is_none();
                let has_track = state.now_playing.is_some();

                if is_local_output && has_track {
                    info!(zone_id, position_ms, "seek_local_output_recreating_stream");
                    self.playback.seek(zone_id, position_ms as i64).await;

                    // Stop the current output FIRST so the old ASIO/WASAPI
                    // thread releases the device before play() creates a new
                    // stream. Without this, the old thread may still hold the
                    // HTTP connection when the new session starts, causing a
                    // "request or response body error" race condition.
                    if let Some(ref did) = state.now_playing.as_ref().and_then(|_| {
                        ZoneRepo::with_backend(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .and_then(|z| z.output_device_id)
                    }) {
                        if did.starts_with("local:") {
                            let outputs = self.outputs.lock().await;
                            if let Some(output) = outputs.get(did.as_str()) {
                                let _ = output.lock().await.stop().await;
                            }
                            drop(outputs);
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        }
                    }

                    let np = state.now_playing.as_ref().unwrap();
                    let output_device_id = ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id);
                    let req = PlayRequest {
                        zone_id,
                        output_device_id,
                        track_id: np.track_id,
                        source: Some(np.source.clone()),
                        source_id: np.source_id.clone(),
                        title: Some(np.title.clone()),
                        artist_name: np.artist_name.clone(),
                        album_title: np.album_title.clone(),
                        cover_url: np.cover_path.clone(),
                        duration_ms: Some(np.duration_ms),
                        seek_ms: Some(position_ms),
                        temp_file_path: None,
                    };

                    match self.play(req).await {
                        Ok(_) => {
                            self.playback.seek(zone_id, position_ms as i64).await;
                            info!(
                                zone_id,
                                position_ms,
                                seek_ms = seek_start.elapsed().as_millis() as u64,
                                "seek_local_output_complete"
                            );
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "seek_local_output_play_failed");
                            self.playback.seek(zone_id, position_ms as i64).await;
                        }
                    }
                } else {
                    let outputs = self.outputs.lock().await;
                    if let Some(output) = outputs.get(did) {
                        if let Err(e) = output.lock().await.seek(position_ms).await {
                            warn!(zone_id, error = %e, "device_seek_failed");
                        }
                    }
                }
            }
        }
    }

    pub async fn set_volume(&self, zone_id: i64, volume: f64, device_id: Option<&str>) {
        // When fixed_volume is enabled, pin volume to 1.0 (bit-perfect) and
        // skip sending to the device — the DAC/renderer handles volume.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        if zone.as_ref().is_some_and(|z| z.fixed_volume) {
            self.playback.set_volume(zone_id, 1.0).await;
            return;
        }

        self.playback.set_volume(zone_id, volume).await;
        self.playback.mark_volume_changed(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                info!(
                    zone_id,
                    volume,
                    device_id = did,
                    "device_set_volume_sending"
                );
                if let Err(e) = output.lock().await.set_volume(volume).await {
                    warn!(zone_id, error = %e, "device_set_volume_failed");
                }
            } else {
                warn!(
                    zone_id,
                    device_id = did,
                    "device_set_volume_output_not_found"
                );
            }
        } else {
            info!(zone_id, volume, "set_volume_no_device_id");
        }
    }

    pub async fn set_mute(&self, zone_id: i64, muted: bool, device_id: Option<&str>) {
        self.playback.set_mute(zone_id, muted).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.set_mute(muted).await {
                    warn!(zone_id, error = %e, "device_set_mute_failed");
                }
            }
        }
    }

    /// Clear the prefetch buffer. Should be called when the queue changes
    /// (add/remove/reorder) so stale prefetched data is discarded.
    pub async fn clear_prefetch(&self) {
        self.prefetch.clear().await;
    }

    /// Persist the play_queue table for a zone with the given local track IDs.
    /// Called after queue mutations to keep the DB in sync with in-memory state.
    pub fn persist_local_queue(&self, zone_id: i64, track_ids: &[i64], current_position: i64) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_queue(zone_id, track_ids) {
            warn!(zone_id, error = %e, "persist_local_queue_failed");
            return;
        }
        if current_position > 0 {
            repo.set_current(zone_id, current_position).ok();
        }
    }

    /// Persist the streaming_queue table for a zone.
    pub fn persist_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )],
    ) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_streaming_queue(zone_id, tracks) {
            warn!(zone_id, error = %e, "persist_streaming_queue_failed");
        }
    }

    pub async fn play_from_queue(&self, zone_id: i64, position: i64) -> Result<PlayResult, String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);

        // Try local queue first
        queue_repo.set_current(zone_id, position).ok();
        let queue = queue_repo.get_queue(zone_id)?;
        if let Some(item) = queue.iter().find(|i| i.is_current) {
            let req = PlayRequest {
                zone_id,
                output_device_id,
                track_id: Some(item.track_id),
                source: None,
                source_id: None,
                title: item.title.clone(),
                artist_name: item.artist_name.clone(),
                album_title: item.album_title.clone(),
                cover_url: item.cover_path.clone(),
                duration_ms: item.duration_ms,
                seek_ms: None,
                temp_file_path: None,
            };
            let result = self.play(req).await?;
            self.playback
                .update_queue_info(zone_id, position, queue.len() as i64)
                .await;
            return Ok(result);
        }

        // Fallback to streaming queue
        let streaming = queue_repo.get_streaming_queue(zone_id)?;
        let item = streaming
            .get(position as usize)
            .ok_or("no queue item at position")?;

        let source_id = item["source_id"].as_str().unwrap_or("").to_string();
        let title = item["title"].as_str().map(String::from);
        let artist = item["artist_name"].as_str().map(String::from);
        let album = item["album_title"].as_str().map(String::from);
        let cover = item["cover_path"].as_str().map(String::from);
        let duration = item["duration_ms"].as_i64();

        // Use the stored source from the streaming_queue, falling back to
        // the current now_playing source (handles old DB rows without source).
        let source = if let Some(s) = item["source"].as_str() {
            s.to_string()
        } else {
            let current_state = self.playback.get_state(zone_id).await;
            current_state
                .now_playing
                .as_ref()
                .map(|np| np.source.clone())
                .unwrap_or_else(|| "tidal".into())
        };

        let req = PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source),
            source_id: Some(source_id),
            title,
            artist_name: artist,
            album_title: album,
            cover_url: cover,
            duration_ms: duration,
            seek_ms: None,
            temp_file_path: None,
        };

        let result = self.play(req).await?;
        self.playback
            .update_queue_info(zone_id, position, streaming.len() as i64)
            .await;
        Ok(result)
    }

    pub async fn advance_queue_metadata(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());
        queue_repo.set_current(zone_id, position).ok();

        let queue = queue_repo.get_queue(zone_id)?;
        if let Some(item) = queue.iter().find(|i| i.is_current) {
            let track_repo = crate::db::track_repo::TrackRepo::with_backend(self.db.clone());
            let track = track_repo.get(item.track_id).ok().flatten();
            let cover_path = track.as_ref().and_then(|t| t.cover_path.clone());
            let np = crate::playback::NowPlaying {
                track_id: Some(item.track_id),
                title: item.title.clone().unwrap_or_default(),
                artist_name: item.artist_name.clone(),
                album_title: item.album_title.clone(),
                cover_path: self.resolve_cover_url(cover_path.as_deref()),
                duration_ms: item.duration_ms.unwrap_or(0),
                source: "local".into(),
                source_id: None,
                stream_id: None,
                format: track.as_ref().and_then(|t| t.format.clone()),
                sample_rate: track.as_ref().and_then(|t| t.sample_rate.map(|v| v as u32)),
                bit_depth: track.as_ref().and_then(|t| t.bit_depth.map(|v| v as u32)),
                genre: track.as_ref().and_then(|t| t.genre.clone()),
                year: track.as_ref().and_then(|t| t.year),
            };
            // Use update_now_playing (not play) to avoid bumping
            // track_generation — the poller must keep its gapless_cooldown
            // intact so it doesn't falsely detect track-end on renderers
            // that briefly report Stopped during gapless transitions.
            self.playback.update_now_playing(zone_id, np).await;
            // Reset position to 0 — the new track starts from the beginning.
            // Without this, the UI shows the cumulative position from the
            // previous track until the next poller tick overwrites it.
            self.playback.update_position(zone_id, 0).await;
            self.playback.emit_position(zone_id, 0);
            self.playback
                .update_queue_info(zone_id, position, queue.len() as i64)
                .await;
            return Ok(());
        }

        let streaming = queue_repo.get_streaming_queue(zone_id)?;
        if let Some(item) = streaming.get(position as usize) {
            let title = item["title"].as_str().unwrap_or("").to_string();
            let artist = item["artist_name"].as_str().map(String::from);
            let album = item["album_title"].as_str().map(String::from);
            let cover = item["cover_path"].as_str().map(String::from);
            let duration = item["duration_ms"].as_i64().unwrap_or(0);
            let source = if let Some(s) = item["source"].as_str() {
                s.to_string()
            } else {
                let cs = self.playback.get_state(zone_id).await;
                cs.now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .unwrap_or_else(|| "streaming".into())
            };
            let np = crate::playback::NowPlaying {
                track_id: None,
                title,
                artist_name: artist,
                album_title: album,
                cover_path: self.resolve_cover_url(cover.as_deref()),
                duration_ms: duration,
                source,
                source_id: item["source_id"].as_str().map(String::from),
                stream_id: None,
                ..Default::default()
            };
            // Same rationale: gapless metadata-only advance must not
            // bump track_generation — but position MUST reset to 0
            // because the new track starts from the beginning.
            self.playback.update_now_playing(zone_id, np).await;
            self.playback.update_position(zone_id, 0).await;
            self.playback.emit_position(zone_id, 0);
            self.playback
                .update_queue_info(zone_id, position, streaming.len() as i64)
                .await;
            return Ok(());
        }

        Err("no queue item at position".into())
    }

    pub async fn resolve_queue_item_url(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<ResolvedQueueItem, String> {
        // Clean up any previously prepared gapless session for this zone
        // before creating a new one.
        self.cleanup_gapless_session(zone_id).await;

        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        // Try local queue first
        let queue = queue_repo.get_queue(zone_id)?;
        if let Some(item) = queue.iter().find(|i| i.position == position) {
            let album = item.album_title.clone();
            let cover = item.cover_path.clone();
            let req = PlayRequest {
                zone_id,
                output_device_id: None,
                track_id: Some(item.track_id),
                source: None,
                source_id: None,
                title: item.title.clone(),
                artist_name: item.artist_name.clone(),
                album_title: album.clone(),
                cover_url: cover.clone(),
                duration_ms: item.duration_ms,
                seek_ms: None,
                temp_file_path: None,
            };
            let resolved = self.resolve_stream(&req).await?;
            if let Some(ref sid) = resolved.stream_id {
                self.gapless_sessions
                    .lock()
                    .await
                    .insert(zone_id, sid.clone());
            }
            let raw_cover = cover.or(resolved.cover_url);
            return Ok(ResolvedQueueItem {
                url: resolved.url,
                mime_type: resolved.mime_type,
                title: resolved.title,
                artist: resolved.artist,
                album,
                cover_url: self.resolve_cover_url(raw_cover.as_deref()),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                stream_id: resolved.stream_id,
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
                file_size: resolved.file_size,
            });
        }

        // Fallback to streaming queue (Tidal, Qobuz, Deezer, etc.)
        let streaming = queue_repo.get_streaming_queue(zone_id)?;
        let item = streaming
            .get(position as usize)
            .ok_or("no queue item at position (local or streaming)")?;
        let source_id = item["source_id"].as_str().unwrap_or("").to_string();
        let title = item["title"].as_str().map(String::from);
        let artist = item["artist_name"].as_str().map(String::from);
        let album = item["album_title"].as_str().map(String::from);
        let cover = item["cover_path"].as_str().map(String::from);
        let duration = item["duration_ms"].as_i64();
        let source = if let Some(s) = item["source"].as_str() {
            s.to_string()
        } else {
            let cs = self.playback.get_state(zone_id).await;
            cs.now_playing
                .as_ref()
                .map(|np| np.source.clone())
                .unwrap_or_else(|| "tidal".into())
        };
        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let req = PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source),
            source_id: Some(source_id),
            title,
            artist_name: artist,
            album_title: album.clone(),
            cover_url: cover.clone(),
            duration_ms: duration,
            seek_ms: None,
            temp_file_path: None,
        };
        let resolved = self.resolve_stream(&req).await?;
        if let Some(ref sid) = resolved.stream_id {
            self.gapless_sessions
                .lock()
                .await
                .insert(zone_id, sid.clone());
        }
        let raw_cover = cover.or(resolved.cover_url);
        Ok(ResolvedQueueItem {
            url: resolved.url,
            mime_type: resolved.mime_type,
            title: resolved.title,
            artist: resolved.artist,
            album,
            cover_url: self.resolve_cover_url(raw_cover.as_deref()),
            duration_ms: resolved.duration_ms.map(|d| d as u64),
            stream_id: resolved.stream_id,
            sample_rate: resolved.sample_rate,
            bit_depth: resolved.bit_depth,
            channels: resolved.channels,
            file_size: resolved.file_size,
        })
    }

    pub async fn wait_stream_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        self.streamer.wait_data_ready(stream_id, timeout_ms).await
    }

    pub async fn streamer_bytes_sent(&self, stream_id: &str) -> Option<u64> {
        self.streamer.stream_bytes_sent(stream_id).await
    }

    async fn persist_position(&self, zone_id: i64) {
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            ZoneRepo::with_backend(self.db.clone())
                .save_playback_position(
                    zone_id,
                    state.position_ms,
                    np.track_id,
                    Some(np.source.as_str()),
                    np.source_id.as_deref(),
                )
                .ok();
        }
    }
}

fn guess_mime_from_url(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") {
        "audio/mpeg"
    } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") {
        "audio/mp4"
    } else if path.ends_with(".ogg") || path.ends_with(".opus") {
        "audio/ogg"
    } else if path.ends_with(".flac") {
        "audio/flac"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
}

/// Decode an infinite radio HTTP stream to PCM and send chunks through the
/// session channel.  Runs on a blocking thread (called via spawn_blocking).
///
/// Uses symphonia with `ReadOnlySource` to handle the non-seekable HTTP stream.
/// Decodes packets progressively and converts to interleaved 16-bit PCM bytes.
/// The loop runs until the stream ends, the sender is dropped (stop), or an
/// unrecoverable error occurs.
fn decode_radio_stream_to_pcm(
    url: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    use symphonia::core::audio::conv::IntoSample;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use symphonia::core::meta::MetadataOptions;
    use tracing::{debug, info, warn};

    // Open HTTP connection — no total timeout for infinite radio streams
    let response = reqwest::blocking::Client::builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .and_then(|c| c.get(&url).send())
        .map_err(|e| format!("radio HTTP fetch failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("radio HTTP error: {}", response.status()));
    }

    info!(url = %url, "radio_local_decode_stream_connected");

    // Wrap the HTTP response body (Read-only, non-seekable) for symphonia
    let source = ReadOnlySource::new(response);
    let mss = MediaSourceStream::new(Box::new(source), Default::default());

    // Provide a hint based on the URL extension to help probe
    let mut hint = Hint::new();
    let lower = url.to_lowercase();
    let path_part = lower.split('?').next().unwrap_or(&lower);
    if path_part.ends_with(".mp3") {
        hint.with_extension("mp3");
    } else if path_part.ends_with(".aac") || path_part.ends_with(".m4a") {
        hint.with_extension("aac");
    } else if path_part.ends_with(".ogg") {
        hint.with_extension("ogg");
    } else if path_part.ends_with(".flac") {
        hint.with_extension("flac");
    } else {
        // Most radio streams are MP3 or AAC; default to mp3 hint
        hint.with_extension("mp3");
    }

    let mut format: Box<dyn symphonia::core::formats::FormatReader> =
        symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| format!("radio probe failed: {e}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("radio stream: no audio track found")?;

    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("radio stream: no audio codec parameters".into()),
    };
    let track_id = track.id;
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let source_sample_rate = audio_params.sample_rate.unwrap_or(44100);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("radio decoder init failed: {e}"))?;

    info!(
        channels = source_channels,
        sample_rate = source_sample_rate,
        "radio_local_decode_started"
    );

    let rt =
        tokio::runtime::Handle::try_current().map_err(|_| "no tokio runtime for radio decode")?;

    let mut first_chunk_sent = false;
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(65536);
    let chunk_size: usize = 32768;

    loop {
        if tx.is_closed() {
            debug!("radio_local_decode_channel_closed_before_packet");
            return Ok(());
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break, // Stream ended (unlikely for radio)
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                debug!("radio_local_decode_eof");
                break;
            }
            Err(e) => {
                // For radio streams, transient errors can occur.
                // Log and break — the orchestrator will handle reconnection
                // if the user restarts playback.
                warn!(error = %e, "radio_local_decode_packet_error");
                break;
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                debug!(error = %e, "radio_local_decode_frame_skip");
                continue;
            }
        };

        // Convert decoded audio buffer to interleaved 16-bit PCM bytes
        let channels = decoded.spec().channels().count();
        let frames = decoded.frames();
        let mut packet_buf: Vec<u8> = Vec::with_capacity(frames * channels * 2);

        // Get samples as interleaved f32, then convert to i16 LE bytes
        let mut interleaved: Vec<f32> = Vec::with_capacity(frames * channels);
        decoded.copy_to_vec_interleaved::<f32>(&mut interleaved);

        for sample in &interleaved {
            let s16: i16 = (*sample).into_sample();
            packet_buf.extend_from_slice(&s16.to_le_bytes());
        }

        pcm_buf.extend_from_slice(&packet_buf);

        // Flush accumulated PCM when we have enough
        while pcm_buf.len() >= chunk_size {
            let chunk: Vec<u8> = pcm_buf.drain(..chunk_size).collect();
            if rt.block_on(tx.send(chunk)).is_err() {
                // Receiver dropped — playback was stopped
                debug!("radio_local_decode_consumer_dropped");
                return Ok(());
            }
            if !first_chunk_sent {
                first_chunk_sent = true;
                data_ready.notify_one();
            }
        }
    }

    // Flush remaining PCM
    if !pcm_buf.is_empty() {
        let _ = rt.block_on(tx.send(pcm_buf));
        if !first_chunk_sent {
            data_ready.notify_one();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::{NowPlaying, PlayState, PlaybackManager};
    use crate::streaming::registry::ServiceRegistry;

    use super::PlaybackOrchestrator;

    fn test_orchestrator() -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    #[tokio::test]
    async fn test_pause_resume_stop() {
        let orch = test_orchestrator();
        let zone_id = 1;

        // Set up a NowPlaying so pause/stop have state to work with
        let np = NowPlaying {
            track_id: Some(42),
            title: "Test Track".into(),
            artist_name: Some("Test Artist".into()),
            album_title: Some("Test Album".into()),
            cover_path: None,
            duration_ms: 180_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Pause
        orch.pause(zone_id, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Paused);

        // Resume
        orch.resume(zone_id, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Playing);

        // Stop
        orch.stop(zone_id, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Stopped);
    }

    #[tokio::test]
    async fn test_seek_persists() {
        let orch = test_orchestrator();

        // Create a zone in the DB so save_playback_position has a row to UPDATE
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Test Zone", None, None).unwrap();

        // Set up NowPlaying (seek persists position only when now_playing exists)
        let np = NowPlaying {
            track_id: Some(99),
            title: "Seek Test".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 300_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Seek to 42 seconds
        orch.seek(zone_id, 42_000, None).await;

        // Verify in-memory state updated
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.position_ms, 42_000);

        // Verify DB position saved
        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 42_000);
        assert_eq!(zone.last_track_id, Some(99));
        assert_eq!(zone.last_track_source.as_deref(), Some("local"));
    }

    #[tokio::test]
    async fn test_set_volume() {
        let orch = test_orchestrator();
        let zone_id = 1;

        // Initialize zone state with a NowPlaying
        let np = NowPlaying {
            track_id: None,
            title: "Volume Test".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 60_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Set volume to 80%
        orch.set_volume(zone_id, 0.8, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 0.8).abs() < f64::EPSILON);

        // Set volume to 0 (mute level)
        orch.set_volume(zone_id, 0.0, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 0.0).abs() < f64::EPSILON);

        // Set volume to 1.0 (max)
        orch.set_volume(zone_id, 1.0, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_never_upsamples() {
        // The Resampler enforces "never upsample": when target > source,
        // the actual target is clamped to source rate/depth.
        use crate::audio::resampler::Resampler;

        // 44.1kHz source, 96kHz target requested -> should stay at 44.1kHz
        let r = Resampler::new(44100, 96000, 16, 24, 2);
        assert_eq!(r.output_rate(), 44100, "must not upsample rate");
        assert_eq!(r.output_depth(), 16, "must not upsample bit depth");
        assert!(
            !r.needs_resample(),
            "no resample needed when target > source"
        );

        // 96kHz source, 48kHz target -> should downsample to 48kHz
        let r = Resampler::new(96000, 48000, 24, 16, 2);
        assert_eq!(r.output_rate(), 48000);
        assert_eq!(r.output_depth(), 16);
        assert!(r.needs_resample(), "downsample should be flagged");

        // Same rate -> no resample
        let r = Resampler::new(48000, 48000, 24, 24, 2);
        assert!(!r.needs_resample());

        // Mixed: rate up but depth down -> rate clamped, depth reduced
        let r = Resampler::new(44100, 96000, 24, 16, 2);
        assert_eq!(r.output_rate(), 44100, "rate must not increase");
        assert_eq!(r.output_depth(), 16, "depth correctly reduced");
        assert!(r.needs_resample(), "bit depth change requires resample");
    }

    #[tokio::test]
    async fn test_persist_position_on_pause() {
        let orch = test_orchestrator();

        // Create a zone in DB
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Pause Zone", None, None).unwrap();

        // Set up playback at a known position
        let np = NowPlaying {
            track_id: Some(7),
            title: "Pause Persist".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 200_000,
            source: "local".into(),
            source_id: Some("src-7".into()),
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;
        orch.playback.update_position(zone_id, 55_000).await;

        // Pause triggers persist_position
        orch.pause(zone_id, None).await;

        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 55_000);
        assert_eq!(zone.last_track_id, Some(7));
        assert_eq!(zone.last_track_source_id.as_deref(), Some("src-7"));
    }

    #[tokio::test]
    async fn test_persist_position_on_stop() {
        let orch = test_orchestrator();

        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Stop Zone", None, None).unwrap();

        let np = NowPlaying {
            track_id: Some(10),
            title: "Stop Persist".into(),
            artist_name: Some("Artist".into()),
            album_title: None,
            cover_path: None,
            duration_ms: 120_000,
            source: "tidal".into(),
            source_id: Some("tidal-10".into()),
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;
        orch.playback.update_position(zone_id, 90_000).await;

        // Stop also persists position
        orch.stop(zone_id, None).await;

        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 90_000);
        assert_eq!(zone.last_track_source.as_deref(), Some("tidal"));
    }

    #[tokio::test]
    async fn test_record_listen() {
        use crate::db::history_repo::HistoryRepo;

        let orch = test_orchestrator();

        // Create a zone so the FK constraint on zone_id is satisfied
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Listen Zone", None, None).unwrap();

        orch.record_listen(
            "Test Song",
            Some("Artist"),
            Some("Album"),
            "local",
            None,
            None,
            180_000,
            zone_id,
            None,
        );

        let repo = HistoryRepo::with_backend(orch.db.clone());
        let history = repo.recent(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Test Song");
        assert_eq!(history[0].artist_name.as_deref(), Some("Artist"));
        assert_eq!(history[0].source, "local");
    }

    #[tokio::test]
    async fn test_resolve_cover_url_passthrough() {
        let orch = test_orchestrator();
        let result = orch.resolve_cover_url(Some("https://img.tidal.com/cover.jpg"));
        assert_eq!(result.as_deref(), Some("https://img.tidal.com/cover.jpg"));

        let result = orch.resolve_cover_url(Some("http://local/art.png"));
        assert_eq!(result.as_deref(), Some("http://local/art.png"));
    }

    #[tokio::test]
    async fn test_resolve_cover_url_hash() {
        let orch = test_orchestrator();
        let result = orch.resolve_cover_url(Some("abc123def"));
        let url = result.unwrap();
        assert!(
            url.contains("/api/v1/library/artwork/abc123def"),
            "got: {url}"
        );
        assert!(url.starts_with("http://"), "got: {url}");
    }

    #[tokio::test]
    async fn test_resolve_cover_url_none() {
        let orch = test_orchestrator();
        assert!(orch.resolve_cover_url(None).is_none());
    }

    #[tokio::test]
    async fn test_persist_local_queue() {
        use crate::db::play_queue_repo::PlayQueueRepo;

        let orch = test_orchestrator();
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Queue Zone", None, None).unwrap();

        // Insert some tracks so FK constraints are satisfied
        orch.db
            .execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
            .unwrap();
        orch.db
            .execute(
                "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
                &[],
            )
            .unwrap();
        for i in 1..=3i64 {
            let title = format!("Track {i}");
            orch.db
                .execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) VALUES (?, ?, 1, 1, 180000)",
                    &[&i as &dyn crate::db::backend::ToSqlValue, &title as &dyn crate::db::backend::ToSqlValue],
                )
                .unwrap();
        }

        orch.persist_local_queue(zone_id, &[1, 2, 3], 0);

        let queue_repo = PlayQueueRepo::with_backend(orch.db.clone());
        let queue = queue_repo.get_queue(zone_id).unwrap();
        assert_eq!(queue.len(), 3);
    }

    #[tokio::test]
    async fn radio_resolve_creates_proxy_session() {
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("radio".into()),
            source_id: Some("http://icecast.radiofrance.fr/fip-hifi.aac".into()),
            title: Some("FIP".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_some(),
            "radio must create a proxy session"
        );
        assert!(
            resolved.url.contains("/stream/"),
            "radio URL must be proxied through local streamer, got: {}",
            resolved.url
        );
        assert!(
            !resolved.url.contains("icecast.radiofrance.fr"),
            "radio URL must NOT be the raw external URL"
        );
    }

    #[tokio::test]
    async fn podcast_resolve_returns_raw_url() {
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("podcast".into()),
            source_id: Some("https://cdn.podcast.com/episode.mp3".into()),
            title: Some("Episode 1".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(3600000),
            seek_ms: None,
            temp_file_path: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_none(),
            "podcast should not create proxy session"
        );
        assert_eq!(resolved.url, "https://cdn.podcast.com/episode.mp3");
    }
}
