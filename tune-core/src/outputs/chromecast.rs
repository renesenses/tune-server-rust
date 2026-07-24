use tracing::info;

use super::traits::{OutputStatus, OutputTarget, TransportState};

/// rust_cast opens a plain blocking `TcpStream` with no connect/read timeout:
/// a Chromecast that vanished from the network (sleep, Wi-Fi drop — some flap
/// every few minutes) turns that connect into a minutes-long hang that
/// strands a blocking-pool thread. Probe with a bounded connect first so a
/// dead host fails fast.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `true` when `host:port` accepts a TCP connection within `timeout`. Returns
/// `true` on resolution failure so rust_cast surfaces the real error itself.
fn probe_reachable(host: &str, port: u16, timeout: std::time::Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    match (host, port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => TcpStream::connect_timeout(&addr, timeout).is_ok(),
            None => true,
        },
        Err(_) => true,
    }
}

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

    /// Chromecast does not consume `set_next_media` (no cast-queue / autoplay
    /// staging is implemented — `set_next_url` is the no-op default). Returning
    /// true here made the poller arm the gapless guard, which orphaned the
    /// staged track and suppressed the natural-end advance: playback stalled
    /// ~30-60s at every track boundary (Rhorn, Chromecast Audio, forum #1072).
    /// Rely on the poller's natural-end fallback instead, like slimproto.
    fn supports_internal_gapless(&self) -> bool {
        false
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
            if !probe_reachable(&host, port, PROBE_TIMEOUT) {
                return Ok(OutputStatus::default());
            }
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
                    ended_naturally: false,
                    volume,
                    muted,
                    ..Default::default()
                });
            };

            if device.connection.connect(&app.transport_id).is_err() {
                return Ok(OutputStatus {
                    ended_naturally: false,
                    volume,
                    muted,
                    ..Default::default()
                });
            }

            let media_status = match device.media.get_status(&app.transport_id, None) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(OutputStatus {
                        ended_naturally: false,
                        volume,
                        muted,
                        ..Default::default()
                    });
                }
            };

            let Some(entry) = media_status.entries.first() else {
                return Ok(OutputStatus {
                    ended_naturally: false,
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

            // The receiver reports `idle_reason = FINISHED` when a track played
            // to its end (vs CANCELLED / INTERRUPTED / ERROR). Surface that as
            // `ended_naturally` so the poller advances to the next track right
            // away. Without it, every FINISHED looked like a plain Stopped state
            // and the poller only advanced via its 30 s wall-clock fallback —
            // Chromecast albums stalled 30-60 s between tracks (#1072, Rhorn).
            let ended_naturally = matches!(
                entry.idle_reason,
                Some(rust_cast::channels::media::IdleReason::Finished)
            );

            Ok(OutputStatus {
                state,
                position_ms,
                duration_ms,
                volume,
                muted,
                current_uri,
                track_title: None,
                track_artist: None,
                ended_naturally,
            })
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn is_available(&self) -> bool {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            probe_reachable(&host, port, PROBE_TIMEOUT)
                && rust_cast::CastDevice::connect_without_host_verification(&host, port).is_ok()
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn reachable_host_probes_true() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_reachable(
            "127.0.0.1",
            port,
            std::time::Duration::from_millis(500)
        ));
    }

    #[test]
    fn dead_host_probes_false_fast() {
        // Bind then drop: the port is closed, connect is refused immediately.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let start = std::time::Instant::now();
        assert!(!probe_reachable(
            "127.0.0.1",
            port,
            std::time::Duration::from_millis(500)
        ));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn unresolvable_host_falls_through_true() {
        // rust_cast must surface the real error itself.
        assert!(probe_reachable(
            "definitely-not-a-real-host.invalid",
            8009,
            std::time::Duration::from_millis(500)
        ));
    }
}
