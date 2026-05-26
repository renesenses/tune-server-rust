use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::history_repo::{HistoryRepo, ListenRecord};
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
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
}

impl PlaybackOrchestrator {
    pub async fn play(&self, req: PlayRequest) -> Result<PlayResult, String> {
        let (stream_url, mime_type, title, artist, duration_ms, source) =
            self.resolve_stream(&req).await?;

        let np = NowPlaying {
            track_id: req.track_id,
            title: title.clone(),
            artist_name: artist.clone(),
            album_title: req.album_title.clone(),
            cover_path: req.cover_url.clone(),
            duration_ms: duration_ms.unwrap_or(0),
            source: source.clone(),
            source_id: req.source_id.clone(),
            stream_id: None,
        };

        self.playback.play(req.zone_id, np).await;

        let output_sent = if let Some(ref device_id) = req.output_device_id {
            self.send_to_output(device_id, &stream_url, &mime_type, Some(&title), artist.as_deref()).await
        } else {
            false
        };

        self.record_listen(&title, artist.as_deref(), req.album_title.as_deref(), &source, duration_ms.unwrap_or(0), req.zone_id);

        info!(
            zone_id = req.zone_id,
            title = %title,
            source = %source,
            output_sent,
            "orchestrator_play"
        );

        Ok(PlayResult {
            stream_url: Some(stream_url),
            output_sent,
            source,
        })
    }

    async fn resolve_stream(&self, req: &PlayRequest) -> Result<(String, String, String, Option<String>, Option<i64>, String), String> {
        if let Some(ref source) = req.source
            && source != "local" {
                return self.resolve_streaming_url(source, req).await;
            }

        self.resolve_local_track(req).await
    }

    async fn resolve_local_track(&self, req: &PlayRequest) -> Result<(String, String, String, Option<String>, Option<i64>, String), String> {
        let track_id = req.track_id.ok_or("no track_id for local playback")?;
        let repo = TrackRepo::new(self.db.clone());
        let track = repo.get(track_id).map_err(|e| e.to_string())?.ok_or("track not found")?;

        let file_path = track.file_path.ok_or("track has no file_path")?;
        let fmt = track.format.unwrap_or_else(|| "flac".into());
        let mime = crate::audio::formats::AudioFormat::from_extension(&fmt)
            .map(|f| f.mime_type().to_string())
            .unwrap_or_else(|| "audio/flac".into());

        let info = StreamInfo {
            format: fmt.clone(),
            mime_type: mime.clone(),
            sample_rate: track.sample_rate.unwrap_or(44100) as u32,
            bit_depth: track.bit_depth.unwrap_or(16) as u16,
            channels: track.channels as u16,
            file_size: track.file_size.map(|s| s as u64),
        };

        let session_id = self.streamer.create_file_session(info, file_path.clone(), false).await;

        let server_ip = crate::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let ext = &fmt;
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, ext);

        Ok((
            stream_url,
            mime,
            track.title,
            track.artist_name,
            Some(track.duration_ms),
            "local".into(),
        ))
    }

    async fn resolve_streaming_url(&self, service_name: &str, req: &PlayRequest) -> Result<(String, String, String, Option<String>, Option<i64>, String), String> {
        let source_id = req.source_id.as_deref().ok_or("source_id required for streaming")?;

        let registry = self.services.lock().await;
        let svc = registry.get(service_name).ok_or_else(|| format!("unknown service: {service_name}"))?;
        let svc = svc.lock().await;

        let stream_data = svc.get_track_url(source_id, None).await?;

        let info = StreamInfo {
            format: stream_data.quality.codec.to_lowercase(),
            mime_type: stream_data.mime_type.clone(),
            sample_rate: stream_data.quality.sample_rate,
            bit_depth: stream_data.quality.bit_depth,
            channels: 2,
            file_size: None,
        };

        let is_https = stream_data.url.starts_with("https://");
        let stream_url = if is_https {
            let session_id = self.streamer.create_proxy_session(
                info,
                stream_data.url.clone(),
                false,
            ).await;

            let server_ip = crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            self.streamer.get_stream_url(&session_id, &server_ip, &stream_data.quality.codec.to_lowercase())
        } else {
            stream_data.url.clone()
        };

        let (title, artist, duration_ms) = if req.title.is_some() {
            (req.title.clone().unwrap(), req.artist_name.clone(), req.duration_ms)
        } else {
            match svc.get_track(source_id).await {
                Ok(track) => (track.title, Some(track.artist), Some(track.duration_ms as i64)),
                Err(_) => ("Unknown".into(), None, req.duration_ms),
            }
        };

        Ok((stream_url, stream_data.mime_type, title, artist, duration_ms, service_name.into()))
    }

    async fn send_to_output(&self, device_id: &str, url: &str, mime_type: &str, title: Option<&str>, artist: Option<&str>) -> bool {
        let outputs = self.outputs.lock().await;
        if let Some(output) = outputs.get(device_id) {
            let output = output.lock().await;
            match output.play_url(url, mime_type, title, artist).await {
                Ok(()) => {
                    info!(device_id, "output_play_sent");
                    true
                }
                Err(e) => {
                    warn!(device_id, error = %e, "output_play_failed");
                    false
                }
            }
        } else {
            warn!(device_id, "output_not_found");
            false
        }
    }

    fn record_listen(&self, title: &str, artist: Option<&str>, album: Option<&str>, source: &str, duration_ms: i64, zone_id: i64) {
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
        }).ok();
    }

    pub async fn pause(&self, zone_id: i64, device_id: Option<&str>) {
        self.playback.pause(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                output.lock().await.pause().await.ok();
            }
        }
    }

    pub async fn resume(&self, zone_id: i64, device_id: Option<&str>) {
        self.playback.resume(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                output.lock().await.resume().await.ok();
            }
        }
    }

    pub async fn stop(&self, zone_id: i64, device_id: Option<&str>) {
        self.playback.stop(zone_id).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                output.lock().await.stop().await.ok();
            }
        }
    }

    pub async fn seek(&self, zone_id: i64, position_ms: u64, device_id: Option<&str>) {
        self.playback.seek(zone_id, position_ms as i64).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                output.lock().await.seek(position_ms).await.ok();
            }
        }
    }

    pub async fn set_volume(&self, zone_id: i64, volume: f64, device_id: Option<&str>) {
        self.playback.set_volume(zone_id, volume).await;
        if let Some(did) = device_id {
            let outputs = self.outputs.lock().await;
            if let Some(output) = outputs.get(did) {
                output.lock().await.set_volume(volume).await.ok();
            }
        }
    }

    pub async fn play_from_queue(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<PlayResult, String> {
        let queue_repo = PlayQueueRepo::new(self.db.clone());
        queue_repo.set_current(zone_id, position)?;

        let queue = queue_repo.get_queue(zone_id)?;
        let item = queue
            .iter()
            .find(|i| i.is_current)
            .ok_or("no queue item at position")?;

        let output_device_id = ZoneRepo::new(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);

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
        Ok(result)
    }

    pub async fn resolve_queue_item_url(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<(String, String, String, Option<String>), String> {
        let queue_repo = PlayQueueRepo::new(self.db.clone());
        let queue = queue_repo.get_queue(zone_id)?;
        let item = queue
            .iter()
            .find(|i| i.position == position)
            .ok_or("no queue item at position")?;

        let req = PlayRequest {
            zone_id,
            output_device_id: None,
            track_id: Some(item.track_id),
            source: None,
            source_id: None,
            title: item.title.clone(),
            artist_name: item.artist_name.clone(),
            album_title: item.album_title.clone(),
            cover_url: item.cover_path.clone(),
            duration_ms: item.duration_ms,
        };

        let (url, mime, title, artist, _, _) = self.resolve_stream(&req).await?;
        Ok((url, mime, title, artist))
    }
}
