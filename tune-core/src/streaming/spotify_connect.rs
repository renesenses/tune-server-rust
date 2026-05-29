use std::sync::Arc;

use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info};

const PCM_SAMPLE_RATE: u32 = 44100;
const PCM_CHANNELS: u16 = 2;
const PCM_BITS_PER_SAMPLE: u16 = 16;
const DEFAULT_BITRATE: u32 = 320;

pub struct LibrespotDaemon {
    device_name: String,
    binary_path: String,
    bitrate: u32,
    process: Mutex<Option<Child>>,
}

impl LibrespotDaemon {
    pub fn new(device_name: String, binary_path: Option<String>, bitrate: Option<u32>) -> Self {
        Self {
            device_name,
            binary_path: binary_path.unwrap_or_else(|| "librespot".into()),
            bitrate: bitrate.unwrap_or(DEFAULT_BITRATE),
            process: Mutex::new(None),
        }
    }

    pub async fn start<F>(&self, on_event: F) -> Result<(), String>
    where
        F: Fn(String, Option<String>) + Send + 'static,
    {
        let mut proc = Command::new(&self.binary_path)
            .args([
                "--name",
                &self.device_name,
                "--bitrate",
                &self.bitrate.to_string(),
                "--backend",
                "pipe",
                "--device-type",
                "speaker",
                "--disable-audio-cache",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("librespot start: {e}"))?;

        if let Some(stderr) = proc.stderr.take() {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some((event, track_id)) = parse_librespot_event(&line) {
                        on_event(event, track_id);
                    } else {
                        debug!(line = %line, "librespot_stderr");
                    }
                }
            });
        }

        info!(device = %self.device_name, "librespot_started");
        *self.process.lock().await = Some(proc);
        Ok(())
    }

    pub async fn stop(&self) {
        let mut proc = self.process.lock().await;
        if let Some(mut child) = proc.take() {
            let _ = child.kill().await;
            info!("librespot_stopped");
        }
    }

    pub async fn is_running(&self) -> bool {
        let mut proc = self.process.lock().await;
        match proc.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    pub fn pcm_spec() -> (u32, u16, u16) {
        (PCM_SAMPLE_RATE, PCM_CHANNELS, PCM_BITS_PER_SAMPLE)
    }
}

fn parse_librespot_event(line: &str) -> Option<(String, Option<String>)> {
    let lower = line.to_lowercase();
    if !lower.contains("player_event") && !lower.contains("event") {
        return None;
    }

    let events = [
        "playing",
        "started",
        "changed",
        "track_changed",
        "session_connected",
        "stopped",
        "session_disconnected",
        "end_of_track",
        "paused",
    ];

    let event = events.iter().find(|e| lower.contains(*e))?;

    let track_id = line
        .find("track_id")
        .and_then(|pos| {
            let after = &line[pos..];
            let start = after.find(|c: char| c.is_alphanumeric() && c != 't' && c != 'r' && c != 'a' && c != 'c' && c != 'k' && c != '_' && c != 'i' && c != 'd')
                .or_else(|| after.find('=').map(|p| p + 1))
                .or_else(|| after.find(':').map(|p| p + 1))?;
            let trimmed = after[start..].trim_start_matches(|c: char| !c.is_alphanumeric());
            let end = trimmed.find(|c: char| !c.is_alphanumeric()).unwrap_or(trimmed.len());
            if end > 0 {
                Some(trimmed[..end].to_string())
            } else {
                None
            }
        });

    Some((event.to_string(), track_id))
}

pub struct SpotifyConnectManager {
    daemon: Arc<LibrespotDaemon>,
    enabled: Mutex<bool>,
    zone_id: Mutex<Option<i64>>,
    device_name: String,
    relay_port: u16,
}

impl SpotifyConnectManager {
    pub fn new(device_name: String, relay_port: u16) -> Self {
        Self {
            daemon: Arc::new(LibrespotDaemon::new(device_name.clone(), None, None)),
            enabled: Mutex::new(false),
            zone_id: Mutex::new(None),
            device_name,
            relay_port,
        }
    }

    pub async fn enable(&self, zone_id: i64) -> Result<(), String> {
        *self.zone_id.lock().await = Some(zone_id);
        *self.enabled.lock().await = true;
        self.daemon
            .start(|event, track_id| {
                info!(event = %event, track_id = ?track_id, "spotify_connect_event");
            })
            .await?;
        info!(zone_id, "spotify_connect_enabled");
        Ok(())
    }

    pub async fn disable(&self) {
        *self.enabled.lock().await = false;
        *self.zone_id.lock().await = None;
        self.daemon.stop().await;
        info!("spotify_connect_disabled");
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    pub fn stream_url(&self, server_ip: &str) -> String {
        format!(
            "http://{}:{}/spotify-connect/stream.wav",
            server_ip, self.relay_port
        )
    }

    pub async fn status(&self) -> serde_json::Value {
        let enabled = *self.enabled.lock().await;
        let zone_id = *self.zone_id.lock().await;
        let running = self.daemon.is_running().await;
        serde_json::json!({
            "enabled": enabled,
            "device_name": &self.device_name,
            "zone_id": zone_id,
            "active": running,
            "binary_available": binary_available(),
        })
    }
}

pub fn binary_available() -> bool {
    std::process::Command::new("which")
        .arg("librespot")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn build_wav_header() -> Vec<u8> {
    let byte_rate = PCM_SAMPLE_RATE * PCM_CHANNELS as u32 * (PCM_BITS_PER_SAMPLE as u32 / 8);
    let block_align = PCM_CHANNELS * (PCM_BITS_PER_SAMPLE / 8);
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // unknown size
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&PCM_CHANNELS.to_le_bytes());
    header.extend_from_slice(&PCM_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&PCM_BITS_PER_SAMPLE.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // unknown size
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_size() {
        let h = build_wav_header();
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
    }

    #[test]
    fn parse_event_playing() {
        let line = "player_event: playing track_id=abc123";
        let (event, tid) = parse_librespot_event(line).unwrap();
        assert_eq!(event, "playing");
        assert!(tid.is_some());
    }

    #[test]
    fn parse_event_paused() {
        let line = "Event: paused";
        let (event, _) = parse_librespot_event(line).unwrap();
        assert_eq!(event, "paused");
    }

    #[test]
    fn parse_no_event() {
        assert!(parse_librespot_event("some random log line").is_none());
    }

    #[test]
    fn pcm_spec() {
        let (sr, ch, bd) = LibrespotDaemon::pcm_spec();
        assert_eq!(sr, 44100);
        assert_eq!(ch, 2);
        assert_eq!(bd, 16);
    }
}
