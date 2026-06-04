use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use tracing::{debug, info};

use super::traits::*;

/// LMS CLI port (telnet-style protocol). NOT 9000 (JSON-RPC/HTTP).
pub const LMS_CLI_PORT: u16 = 9090;

pub struct SqueezeboxOutput {
    name: String,
    device_id: String,
    player_id: String,
    lms_host: String,
    lms_port: u16,
}

impl SqueezeboxOutput {
    pub fn new(name: String, device_id: String, lms_host: String, lms_port: u16) -> Self {
        let player_id = device_id
            .strip_prefix("squeezebox-")
            .unwrap_or(&device_id)
            .to_string();
        Self {
            name,
            device_id,
            player_id,
            lms_host,
            lms_port,
        }
    }

    /// Send a raw CLI command to LMS and return the response line.
    ///
    /// The LMS CLI protocol is telnet-style on port 9090:
    /// - Commands are newline-terminated
    /// - The server echoes the command back with results appended
    /// - Each connection is stateless (open, send, read, close)
    fn lms_cli_command(&self, cmd: &str) -> Result<String, String> {
        let addr = format!("{}:{}", self.lms_host, self.lms_port);
        let stream = TcpStream::connect_timeout(
            &addr.parse().map_err(|e| format!("invalid LMS address {addr}: {e}"))?,
            Duration::from_secs(5),
        )
        .map_err(|e| {
            format!(
                "LMS CLI connection failed ({addr}): {e}. Check that Logitech Media Server is running."
            )
        })?;

        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| format!("set write timeout: {e}"))?;

        let mut writer = stream
            .try_clone()
            .map_err(|e| format!("clone stream: {e}"))?;
        let line = format!("{cmd}\n");
        writer
            .write_all(line.as_bytes())
            .map_err(|e| format!("LMS CLI write failed: {e}"))?;
        writer.flush().map_err(|e| format!("LMS CLI flush: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .map_err(|e| format!("LMS CLI read failed: {e}"))?;

        let decoded = urlencoding::decode(response.trim())
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| response.trim().to_string());

        Ok(decoded)
    }

    /// Send a player-scoped CLI command.
    /// The player MAC is URL-encoded and prepended to the command.
    fn player_command(&self, cmd: &str) -> Result<String, String> {
        let encoded_mac = urlencoding::encode(&self.player_id);
        let full_cmd = format!("{encoded_mac} {cmd}");
        self.lms_cli_command(&full_cmd)
    }

    /// Query player status via CLI (returns key-value pairs).
    fn player_status_cli(&self) -> Result<Vec<(String, String)>, String> {
        let encoded_mac = urlencoding::encode(&self.player_id);
        let resp = self.lms_cli_command(&format!("{encoded_mac} status 0 100 tags:adlNJ"))?;

        // The response is space-separated key:value pairs (URL-encoded)
        let mut pairs = Vec::new();
        // Strip the player id prefix from the response
        let body = resp
            .strip_prefix(&format!("{} ", self.player_id))
            .unwrap_or(&resp);

        for token in body.split(' ') {
            if let Some((k, v)) = token.split_once(':') {
                pairs.push((k.to_string(), v.to_string()));
            }
        }
        Ok(pairs)
    }

    fn get_status_value(pairs: &[(String, String)], key: &str) -> Option<String> {
        pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }
}

#[async_trait::async_trait]
impl OutputTarget for SqueezeboxOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "squeezebox"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.lms_host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        info!(player = %self.device_id, url = media.url, "squeezebox_play");

        // Power on the player first
        if let Err(e) = self.player_command("power 1") {
            debug!(player = %self.device_id, error = %e, "squeezebox_power_on_failed");
        }

        // URL-encode the stream URL for the CLI
        let encoded_url = urlencoding::encode(media.url);
        self.player_command(&format!("playlist play {encoded_url}"))?;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.player_command("pause 1")?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        // Power on in case the player went to standby
        if let Err(e) = self.player_command("power 1") {
            debug!(player = %self.device_id, error = %e, "squeezebox_power_on_failed");
        }
        self.player_command("pause 0")?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.player_command("stop")?;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let secs = position_ms as f64 / 1000.0;
        self.player_command(&format!("time {secs:.1}"))?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let vol = (volume * 100.0).round().clamp(0.0, 100.0) as u8;
        self.player_command(&format!("mixer volume {vol}"))?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { 1 } else { 0 };
        self.player_command(&format!("mixer muting {val}"))?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let pairs = self.player_status_cli()?;

        let mode = Self::get_status_value(&pairs, "mode").unwrap_or_default();
        let state = match mode.as_str() {
            "play" => TransportState::Playing,
            "pause" => TransportState::Paused,
            _ => TransportState::Stopped,
        };

        let position_ms = Self::get_status_value(&pairs, "time")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let duration_ms = Self::get_status_value(&pairs, "duration")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let volume = Self::get_status_value(&pairs, "mixer volume")
            .or_else(|| Self::get_status_value(&pairs, "mixer_volume"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);

        let current_uri = Self::get_status_value(&pairs, "current_title");
        let track_title = Self::get_status_value(&pairs, "title");
        let track_artist = Self::get_status_value(&pairs, "artist");

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted: false,
            current_uri,
            track_title,
            track_artist,
        })
    }

    async fn is_available(&self) -> bool {
        self.player_status_cli().is_ok()
    }

    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        _title: Option<&str>,
        _artist: Option<&str>,
    ) -> Result<(), String> {
        let encoded_url = urlencoding::encode(url);
        self.player_command(&format!("playlist add {encoded_url}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_port_constant() {
        assert_eq!(LMS_CLI_PORT, 9090);
    }

    #[test]
    fn output_type() {
        let sb = SqueezeboxOutput::new("Test".into(), "id".into(), "localhost".into(), 9090);
        assert_eq!(sb.output_type(), "squeezebox");
    }

    #[test]
    fn player_id_strips_prefix() {
        let sb = SqueezeboxOutput::new(
            "Kitchen".into(),
            "squeezebox-00:04:20:ab:cd:ef".into(),
            "192.168.1.100".into(),
            9090,
        );
        assert_eq!(sb.player_id, "00:04:20:ab:cd:ef");
    }

    #[test]
    fn player_id_no_prefix() {
        let sb = SqueezeboxOutput::new(
            "Kitchen".into(),
            "00:04:20:ab:cd:ef".into(),
            "192.168.1.100".into(),
            9090,
        );
        assert_eq!(sb.player_id, "00:04:20:ab:cd:ef");
    }

    #[test]
    fn mac_url_encoding() {
        let mac = "00:04:20:ab:cd:ef";
        let encoded = urlencoding::encode(mac);
        assert_eq!(encoded, "00%3A04%3A20%3Aab%3Acd%3Aef");
    }
}
