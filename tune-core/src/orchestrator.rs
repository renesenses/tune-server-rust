use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::audio::formats::AudioFormat;
use crate::db::history_repo::{HistoryRepo, ListenRecord};
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

pub struct PlaybackOrchestrator {
    pub db: SqliteDb,
    pub playback: Arc<PlaybackManager>,
    pub streamer: Arc<AudioStreamer>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub advertised_ip: Option<String>,
    pub event_bus: Option<Arc<EventBus>>,
    gapless_sessions: Mutex<HashMap<i64, String>>,
}

#[derive(Debug, Clone)]
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
        db: SqliteDb,
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
        // Ensure output_device_id is populated: if the caller didn't provide
        // it (e.g. web client sends only zone_id + track_id), look it up from
        // the zone's DB record.  This is the primary gate for send_to_output —
        // without it, the stream is created but never sent to the output device.
        if req.output_device_id.is_none() {
            let looked_up = ZoneRepo::new(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id);
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

        let resolved = self.resolve_stream(&req).await?;

        let cover_path = req.cover_url.clone().or(resolved.cover_url.clone());
        let album = req.album_title.clone().or(resolved.album.clone());
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
        };

        self.playback.play(req.zone_id, np).await;

        // Last.fm Now Playing
        self.lastfm_now_playing(&resolved.title, resolved.artist.as_deref());

        // ListenBrainz Now Playing
        self.listenbrainz_now_playing(
            &resolved.title,
            resolved.artist.as_deref(),
            album.as_deref(),
        );

        let (output_sent, output_error) = if let Some(ref device_id) = req.output_device_id {
            let resolved_cover_url = self.resolve_cover_url(cover_path.as_deref());
            let local_file_path = if resolved.source == "local" {
                req.track_id.and_then(|tid| {
                    TrackRepo::new(self.db.clone())
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
            self.send_to_output(device_id, &media).await
        } else {
            warn!(
                zone_id = req.zone_id,
                "no_output_device_id_skipping_send_to_output"
            );
            (false, None)
        };

        // Clean up old session now that the output has been stopped by play_media
        if let Some(ref old_sid) = old_stream_id {
            self.streamer.remove_session(old_sid).await;
        }

        self.record_listen(
            &resolved.title,
            resolved.artist.as_deref(),
            album.as_deref(),
            &resolved.source,
            resolved.duration_ms.unwrap_or(0),
            req.zone_id,
        );

        info!(
            zone_id = req.zone_id,
            title = %resolved.title,
            source = %resolved.source,
            output_sent,
            "orchestrator_play"
        );

        Ok(PlayResult {
            stream_url: Some(resolved.url),
            output_sent,
            source: resolved.source,
            error: output_error,
        })
    }

    async fn resolve_stream(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        if let Some(ref source) = req.source
            && source != "local"
        {
            if source == "podcast" || source == "radio" {
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
        Ok(ResolvedStream {
            url: audio_url.to_string(),
            mime_type: mime_type.to_string(),
            title,
            artist,
            album,
            duration_ms,
            source,
            cover_url,
            stream_id: None,
            file_size: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
        })
    }

    async fn resolve_local_track(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        let track_id = req.track_id.ok_or("no track_id for local playback")?;
        let repo = TrackRepo::new(self.db.clone());
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
        let zone = ZoneRepo::new(self.db.clone())
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
        let needs_transcode_for_output = is_network_output
            && source_format
                .as_ref()
                .is_some_and(|f| f.needs_transcode_for_dlna());
        // Downsample if the zone has a max_sample_rate cap and the source exceeds it
        let needs_downsample = zone_max_sample_rate.is_some_and(|max| sample_rate > max);
        let needs_transcode =
            needs_transcode_for_output || oaat_needs_wav || local_needs_wav || needs_downsample;

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
            let out_bd: u16 = if src_fmt == AudioFormat::Dsd {
                24
            } else if oaat_needs_wav || local_needs_wav {
                bit_depth.max(16).min(24)
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

            let info = StreamInfo {
                format: out_ext.clone(),
                mime_type: out_mime.clone(),
                sample_rate: out_sr,
                bit_depth: out_bd,
                channels,
                file_size: None,
                duration_ms: Some(track.duration_ms as u64),
            };
            // Use the computed WAV/FLAC content length for the DIDL size
            // attribute so DLNA renderers know the correct stream size.
            let transcode_file_size = info.wav_content_length();

            let (session_id, tx, data_ready) = self.streamer.create_session(info, false, 256).await;

            // Native transcoding pipeline: decode with native decoders, encode to target format
            let fp = file_path.clone();
            let target_format_str = if target_fmt == AudioFormat::Wav {
                "wav".to_string()
            } else {
                target_fmt.container_format().to_string()
            };
            let ev_bus = self.event_bus.clone();
            let zone_id = req.zone_id;
            tokio::spawn(async move {
                debug!(file = %fp, sample_rate = out_sr, channels, "transcode_decoding");

                // Create audio-levels channel for ALL transcode paths so the
                // web client VU-meter works regardless of output type.
                let (levels_tx, levels_rx) =
                    std::sync::mpsc::channel::<crate::audio::levels::AudioLevels>();
                if let Some(ref bus) = ev_bus {
                    let bus = bus.clone();
                    tokio::spawn(async move {
                        while let Ok(lvl) = levels_rx.recv() {
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
                                }),
                            );
                        }
                    });
                }

                if target_format_str == "wav" {
                    let fp_clone = fp.clone();
                    let tx_clone = tx.clone();
                    drop(tx);

                    let result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_with_levels(
                            &fp_clone,
                            Some(out_sr),
                            Some(channels as u32),
                            tx_clone,
                            32768,
                            data_ready,
                            levels_tx,
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
                } else {
                    // Non-WAV target (e.g. FLAC encoding): must buffer all PCM
                    // first because the encoder needs the full input to produce
                    // a valid container. Still uses spawn_blocking for I/O.
                    let fp_clone = fp.clone();
                    let decode_result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm(
                            &fp_clone,
                            Some(out_sr),
                            Some(channels as u32),
                            0.0,
                            0.0,
                        )
                    })
                    .await;

                    let decode_result = match decode_result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(error = %e, file = %fp, "transcode_decode_task_panic");
                            return;
                        }
                    };

                    match decode_result {
                        Ok(decoded) => {
                            debug!(
                                samples = decoded.samples_i32.len(),
                                sample_rate = decoded.sample_rate,
                                channels = decoded.channels,
                                bit_depth = decoded.bit_depth,
                                "transcode_decoded"
                            );
                            let pcm_bytes = decoded.pcm_bytes();
                            let actual_bd = decoded.bit_depth;
                            let actual_ch = decoded.channels as u16;

                            // Compute and emit audio levels from PCM before encoding
                            for chunk in pcm_bytes.chunks(32768) {
                                let _ = levels_tx.send(crate::audio::levels::compute_levels(
                                    chunk, actual_bd, actual_ch,
                                ));
                            }
                            drop(levels_tx);

                            // Encode to target format using native bit depth
                            let mut encoder = crate::audio::encoder::AudioEncoder::new(
                                &target_format_str,
                                decoded.sample_rate,
                                actual_bd as u32,
                                decoded.channels,
                            );
                            if let Err(e) = encoder.start().await {
                                warn!(error = %e, file = %fp, "transcode_encoder_start_failed");
                                return;
                            }
                            if let Err(e) = encoder.write(&pcm_bytes).await {
                                warn!(error = %e, file = %fp, "transcode_encoder_write_failed");
                                return;
                            }
                            match encoder.finish().await {
                                Ok(encoded_data) => {
                                    // Send in chunks
                                    for chunk in encoded_data.chunks(32768) {
                                        if tx.send(chunk.to_vec()).await.is_err() {
                                            debug!("transcode_consumer_dropped");
                                            return;
                                        }
                                    }
                                    debug!(file = %fp, "transcode_complete");
                                }
                                Err(e) => {
                                    warn!(error = %e, file = %fp, "transcode_encode_failed");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, file = %fp, "transcode_decode_failed");
                        }
                    }
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
        } else {
            // Standard passthrough: serve the raw file
            let mime = source_format
                .map(|f| f.mime_type().to_string())
                .unwrap_or_else(|| "audio/flac".into());

            let info = StreamInfo {
                format: fmt.clone(),
                mime_type: mime.clone(),
                sample_rate,
                bit_depth,
                channels,
                file_size: track.file_size.map(|s| s as u64),
                duration_ms: Some(track.duration_ms as u64),
            };

            let passthrough_file_size = track.file_size.map(|s| s as u64);

            let session_id = self
                .streamer
                .create_file_session(info, file_path.clone(), false)
                .await;

            // Parallel decode-for-levels: decode the audio in the background
            // purely to emit VU-meter events for the web client. This does not
            // affect the actual audio stream served to the output device.
            if let Some(ref bus) = self.event_bus {
                let bus = bus.clone();
                let fp = file_path.clone();
                let zone_id = req.zone_id;
                let sr = sample_rate;
                let ch = channels as u32;
                tokio::spawn(async move {
                    let (levels_tx, levels_rx) =
                        std::sync::mpsc::channel::<crate::audio::levels::AudioLevels>();
                    let bus_clone = bus.clone();
                    tokio::spawn(async move {
                        while let Ok(lvl) = levels_rx.recv() {
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
                        let decoded =
                            crate::audio::decode::decode_to_pcm(&fp, Some(sr), Some(ch), 0.0, 0.0);
                        if let Ok(ref dec) = decoded {
                            let pcm = dec.pcm_bytes();
                            let bd = dec.bit_depth;
                            let c = dec.channels as u16;
                            for chunk in pcm.chunks(32768) {
                                if levels_tx
                                    .send(crate::audio::levels::compute_levels(chunk, bd, c))
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
        };

        let is_https = stream_data.url.starts_with("https://");
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
        let (stream_url, sid, out_mime) = if is_local_stream || is_oaat_stream {
            let upstream_url = stream_data.url.clone();
            let codec = stream_data.quality.codec.to_lowercase();
            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);

            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: sr,
                bit_depth: bd,
                channels: 2,
                file_size: None,
                duration_ms: None,
            };

            let (session_id, tx, data_ready) =
                self.streamer.create_session(wav_info, false, 256).await;

            info!(
                service = service_name,
                codec = %codec,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_transcode_to_wav_for_local_output"
            );

            let ev_bus = self.event_bus.clone();
            let zone_id = req.zone_id;

            // Background task: download upstream → temp file → decode → WAV → session
            tokio::spawn(async move {
                // Audio-levels channel so the web client VU-meter works for
                // streaming-service content played through local/OAAT outputs.
                let (levels_tx, levels_rx) =
                    std::sync::mpsc::channel::<crate::audio::levels::AudioLevels>();
                if let Some(ref bus) = ev_bus {
                    let bus = bus.clone();
                    tokio::spawn(async move {
                        while let Ok(lvl) = levels_rx.recv() {
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
                                }),
                            );
                        }
                    });
                }

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

                let tmp_file = match download_result {
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
                    Ok(Ok(_bit_depth)) => {
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

            let server_ip = crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (url, Some(session_id), "audio/wav".to_string())
        } else if is_https {
            let session_id = self
                .streamer
                .create_proxy_session(info, stream_data.url.clone(), false)
                .await;
            let server_ip = crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            let url = self.streamer.get_stream_url(
                &session_id,
                &server_ip,
                &stream_data.quality.codec.to_lowercase(),
            );
            (url, Some(session_id), stream_data.mime_type.clone())
        } else {
            (stream_data.url.clone(), None, stream_data.mime_type.clone())
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
            file_size: None,
            sample_rate: Some(stream_data.quality.sample_rate),
            bit_depth: Some(stream_data.quality.bit_depth as u32),
            channels: Some(2),
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

    async fn send_to_output(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
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
            match outputs.get(device_id) {
                Some(arc) => (Some(arc), device_id.to_string()),
                None if device_id.starts_with("local:") => {
                    // The stored local device is no longer registered (e.g. USB
                    // DAC unplugged, macOS renamed the device after update).
                    // Fall back to any other registered local output — the user's
                    // zone should still produce audio rather than silently failing.
                    let fallback = outputs
                        .list()
                        .into_iter()
                        .find(|id| id.starts_with("local:"));
                    if let Some(ref fallback_id) = fallback {
                        warn!(
                            requested = device_id,
                            fallback = %fallback_id,
                            "output_not_found_falling_back_to_other_local — \
                             the configured local output device is no longer available; \
                             using another registered local output"
                        );
                        (outputs.get(fallback_id), fallback_id.clone())
                    } else {
                        (None, device_id.to_string())
                    }
                }
                None => (None, device_id.to_string()),
            }
        };
        if let Some(output_arc) = output_arc {
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

    fn record_listen(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        source: &str,
        duration_ms: i64,
        zone_id: i64,
    ) {
        let repo = HistoryRepo::new(self.db.clone());
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: title.into(),
            artist_name: artist.map(Into::into),
            album_title: album.map(Into::into),
            source: source.into(),
            duration_ms,
            listened_at: None,
            zone_id: Some(zone_id),
        })
        .ok();

        // Last.fm scrobble
        self.lastfm_scrobble(title, artist);

        // ListenBrainz scrobble
        self.listenbrainz_scrobble(title, artist, album);
    }

    fn lastfm_keys(&self) -> Option<(String, String, String)> {
        let settings = SettingsRepo::new(self.db.clone());
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
        let settings = SettingsRepo::new(self.db.clone());
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
        self.playback.resume(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.resume().await {
                    warn!(zone_id, error = %e, "device_resume_failed");
                }
            }
        }
    }

    pub async fn stop(&self, zone_id: i64, device_id: Option<&str>) {
        self.persist_position(zone_id).await;
        self.cleanup_gapless_session(zone_id).await;
        let state = self.playback.get_state(zone_id).await;
        let old_stream_id = state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());
        self.playback.stop(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.stop().await {
                    warn!(zone_id, error = %e, "device_stop_failed");
                }
            }
        }
        // Remove session AFTER the output has been stopped
        if let Some(ref sid) = old_stream_id {
            self.streamer.remove_session(sid).await;
        }
    }

    pub async fn seek(&self, zone_id: i64, position_ms: u64, device_id: Option<&str>) {
        self.playback.seek(zone_id, position_ms as i64).await;
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            if let Err(e) = ZoneRepo::new(self.db.clone()).save_playback_position(
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
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.seek(position_ms).await {
                    warn!(zone_id, error = %e, "device_seek_failed");
                }
            }
        }
    }

    pub async fn set_volume(&self, zone_id: i64, volume: f64, device_id: Option<&str>) {
        self.playback.set_volume(zone_id, volume).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                if let Err(e) = output.lock().await.set_volume(volume).await {
                    warn!(zone_id, error = %e, "device_set_volume_failed");
                }
            }
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

    /// Persist the play_queue table for a zone with the given local track IDs.
    /// Called after queue mutations to keep the DB in sync with in-memory state.
    pub fn persist_local_queue(&self, zone_id: i64, track_ids: &[i64], current_position: i64) {
        let repo = PlayQueueRepo::new(self.db.clone());
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
        let repo = PlayQueueRepo::new(self.db.clone());
        if let Err(e) = repo.set_streaming_queue(zone_id, tracks) {
            warn!(zone_id, error = %e, "persist_streaming_queue_failed");
        }
    }

    pub async fn play_from_queue(&self, zone_id: i64, position: i64) -> Result<PlayResult, String> {
        let queue_repo = PlayQueueRepo::new(self.db.clone());

        let output_device_id = ZoneRepo::new(self.db.clone())
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
        };

        let result = self.play(req).await?;
        self.playback
            .update_queue_info(zone_id, position, streaming.len() as i64)
            .await;
        Ok(result)
    }

    pub async fn advance_queue_metadata(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let queue_repo = PlayQueueRepo::new(self.db.clone());
        queue_repo.set_current(zone_id, position).ok();

        let queue = queue_repo.get_queue(zone_id)?;
        if let Some(item) = queue.iter().find(|i| i.is_current) {
            let track_repo = crate::db::track_repo::TrackRepo::new(self.db.clone());
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
            };
            // Use update_now_playing (not play) to avoid bumping
            // track_generation — the poller must keep its gapless_cooldown
            // intact so it doesn't falsely detect track-end on renderers
            // that briefly report Stopped during gapless transitions.
            self.playback.update_now_playing(zone_id, np).await;
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
            };
            // Same rationale: gapless metadata-only advance must not
            // bump track_generation and must not reset position to 0.
            self.playback.update_now_playing(zone_id, np).await;
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

        let queue_repo = PlayQueueRepo::new(self.db.clone());

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
        let output_device_id = ZoneRepo::new(self.db.clone())
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

    async fn persist_position(&self, zone_id: i64) {
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            ZoneRepo::new(self.db.clone())
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
        let zone_repo = ZoneRepo::new(orch.db.clone());
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
        let zone_repo = ZoneRepo::new(orch.db.clone());
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

        let zone_repo = ZoneRepo::new(orch.db.clone());
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
        let zone_repo = ZoneRepo::new(orch.db.clone());
        let zone_id = zone_repo.create("Listen Zone", None, None).unwrap();

        orch.record_listen(
            "Test Song",
            Some("Artist"),
            Some("Album"),
            "local",
            180_000,
            zone_id,
        );

        let repo = HistoryRepo::new(orch.db.clone());
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
        let zone_repo = ZoneRepo::new(orch.db.clone());
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
            orch.db
                .execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) VALUES (?, ?, 1, 1, 180000)",
                    &[&i, &format!("Track {i}") as &dyn rusqlite::types::ToSql],
                )
                .unwrap();
        }

        orch.persist_local_queue(zone_id, &[1, 2, 3], 0);

        let queue_repo = PlayQueueRepo::new(orch.db.clone());
        let queue = queue_repo.get_queue(zone_id).unwrap();
        assert_eq!(queue.len(), 3);
    }
}
