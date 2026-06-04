use tracing::info;

use super::traits::{OutputStatus, OutputTarget, TransportState};

pub struct ChromecastOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
}

impl ChromecastOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for ChromecastOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "chromecast"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        self.play_url(media.url, media.mime_type, media.title, media.artist)
            .await
    }

    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        let url = url.to_string();
        let mime = mime_type.to_string();
        let title = title.map(String::from);
        let artist = artist.map(String::from);
        let host = self.host.clone();
        let port = self.port;
        let name = self.name.clone();

        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("chromecast connect: {e}"))?;

            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            let app = device
                .receiver
                .launch_app(&rust_cast::channels::receiver::CastDeviceApp::DefaultMediaReceiver)
                .map_err(|e| format!("launch app: {e}"))?;

            device
                .connection
                .connect(&app.transport_id)
                .map_err(|e| format!("connect transport: {e}"))?;

            device
                .media
                .load(
                    &app.transport_id,
                    &app.session_id,
                    &rust_cast::channels::media::Media {
                        content_id: url.clone(),
                        content_type: mime,
                        stream_type: rust_cast::channels::media::StreamType::Buffered,
                        duration: None,
                        metadata: Some(rust_cast::channels::media::Metadata::MusicTrack(
                            rust_cast::channels::media::MusicTrackMediaMetadata {
                                album_name: None,
                                title,
                                album_artist: None,
                                artist,
                                composer: None,
                                track_number: None,
                                disc_number: None,
                                images: vec![],
                                release_date: None,
                            },
                        )),
                    },
                )
                .map_err(|e| format!("load media: {e}"))?;

            info!(device = %name, url, "chromecast_play");
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .pause(&app.transport_id, entry.media_session_id)
                        .map_err(|e| format!("pause: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn resume(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .play(&app.transport_id, entry.media_session_id)
                        .map_err(|e| format!("play: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn stop(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .receiver
                    .stop_app(&app.session_id)
                    .map_err(|e| format!("stop: {e}"))?;
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let position_secs = position_ms as f32 / 1000.0;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .seek(
                            &app.transport_id,
                            entry.media_session_id,
                            Some(position_secs),
                            None,
                        )
                        .map_err(|e| format!("seek: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let level = volume as f32;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            device
                .receiver
                .set_volume(rust_cast::channels::receiver::Volume {
                    level: Some(level),
                    muted: Some(false),
                })
                .map_err(|e| format!("volume: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            device
                .receiver
                .set_volume(rust_cast::channels::receiver::Volume {
                    level: None,
                    muted: Some(muted),
                })
                .map_err(|e| format!("mute: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = match rust_cast::CastDevice::connect_without_host_verification(&host, port)
            {
                Ok(d) => d,
                Err(_) => return Ok(OutputStatus::default()),
            };
            if device.connection.connect("receiver-0").is_err() {
                return Ok(OutputStatus::default());
            }

            let recv_status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;

            let volume = recv_status.volume.level.unwrap_or(0.5) as f64;
            let muted = recv_status.volume.muted.unwrap_or(false);

            let Some(app) = recv_status.applications.first() else {
                return Ok(OutputStatus {
                    volume,
                    muted,
                    ..Default::default()
                });
            };

            if device.connection.connect(&app.transport_id).is_err() {
                return Ok(OutputStatus {
                    volume,
                    muted,
                    ..Default::default()
                });
            }

            let media_status = match device.media.get_status(&app.transport_id, None) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(OutputStatus {
                        volume,
                        muted,
                        ..Default::default()
                    });
                }
            };

            let Some(entry) = media_status.entries.first() else {
                return Ok(OutputStatus {
                    volume,
                    muted,
                    ..Default::default()
                });
            };

            let state = match entry.player_state {
                rust_cast::channels::media::PlayerState::Playing => TransportState::Playing,
                rust_cast::channels::media::PlayerState::Paused => TransportState::Paused,
                rust_cast::channels::media::PlayerState::Buffering => TransportState::Transitioning,
                _ => TransportState::Stopped,
            };

            let position_ms = entry
                .current_time
                .map(|t| (t as f64 * 1000.0) as u64)
                .unwrap_or(0);
            let duration_ms = entry
                .media
                .as_ref()
                .and_then(|m| m.duration)
                .map(|d| (d * 1000.0) as u64)
                .unwrap_or(0);

            let current_uri = entry.media.as_ref().map(|m| m.content_id.clone());

            Ok(OutputStatus {
                state,
                position_ms,
                duration_ms,
                volume,
                muted,
                current_uri,
                track_title: None,
                track_artist: None,
            })
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn is_available(&self) -> bool {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            rust_cast::CastDevice::connect_without_host_verification(&host, port).is_ok()
        })
        .await
        .unwrap_or(false)
    }
}
