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

    /// Send a raw CLI command to LMS and return the raw (URL-encoded) response line.
    ///
    /// The LMS CLI protocol is telnet-style on port 9090:
    /// - Commands are newline-terminated
    /// - The server echoes the command back with results appended
    /// - Each connection is stateless (open, send, read, close)
    /// - The response is URL-encoded; callers must decode as needed.
    fn lms_cli_command_raw(&self, cmd: &str) -> Result<String, String> {
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

        Ok(response.trim().to_string())
    }

    /// Send a raw CLI command and return the URL-decoded response.
    /// Use for simple commands where the response structure doesn't matter.
    fn lms_cli_command(&self, cmd: &str) -> Result<String, String> {
        let raw = self.lms_cli_command_raw(cmd)?;
        let decoded = urlencoding::decode(&raw)
            .map(|s| s.into_owned())
            .unwrap_or(raw);
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
    ///
    /// The LMS CLI response is space-separated tokens, each URL-encoded.
    /// Within each token, keys and values are separated by `%3A` (encoded colon).
    /// We must split on literal spaces first (token boundaries), then decode
    /// each token individually to preserve multi-word keys like "mixer volume".
    fn player_status_cli(&self) -> Result<Vec<(String, String)>, String> {
        let encoded_mac = urlencoding::encode(&self.player_id);
        let raw_resp =
            self.lms_cli_command_raw(&format!("{encoded_mac} status 0 100 tags:adlNJ"))?;

        // The raw response is space-separated tokens (URL-encoded).
        // Strip the player id prefix (encoded MAC) from the response.
        let encoded_prefix = format!("{encoded_mac} ");
        let body = raw_resp.strip_prefix(&*encoded_prefix).unwrap_or(&raw_resp);

        let mut pairs = Vec::new();
        for token in body.split(' ') {
            // Each token is "key%3Avalue" where %3A is the encoded colon separator.
            // We split on the FIRST %3A to get key and value, then decode each.
            if let Some((raw_k, raw_v)) = token.split_once("%3A").or_else(|| token.split_once(':'))
            {
                let key = urlencoding::decode(raw_k)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| raw_k.to_string());
                let value = urlencoding::decode(raw_v)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| raw_v.to_string());
                pairs.push((key, value));
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
            ended_naturally: false,
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

    /// Simulates parsing a raw LMS CLI status response to verify that
    /// multi-word keys (like "mixer volume") and values with spaces
    /// (like track titles) are correctly decoded.
    #[test]
    fn parse_lms_status_tokens() {
        // Simulated raw LMS CLI response (URL-encoded, space-separated tokens):
        // mixer%20volume%3A75 mode%3Aplay time%3A42.5 duration%3A180.0
        // title%3AMy%20Great%20Song artist%3AThe%20Artist
        let raw_tokens = "mixer%20volume%3A75 mode%3Aplay time%3A42.5 duration%3A180.0 title%3AMy%20Great%20Song artist%3AThe%20Artist";

        let mut pairs = Vec::new();
        for token in raw_tokens.split(' ') {
            if let Some((raw_k, raw_v)) = token.split_once("%3A").or_else(|| token.split_once(':'))
            {
                let key = urlencoding::decode(raw_k)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| raw_k.to_string());
                let value = urlencoding::decode(raw_v)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| raw_v.to_string());
                pairs.push((key, value));
            }
        }

        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "mixer volume")
                .map(|(_, v)| v.as_str()),
            Some("75"),
            "multi-word key 'mixer volume' must be parsed correctly"
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "mode")
                .map(|(_, v)| v.as_str()),
            Some("play")
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "time")
                .map(|(_, v)| v.as_str()),
            Some("42.5")
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "duration")
                .map(|(_, v)| v.as_str()),
            Some("180.0")
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "title")
                .map(|(_, v)| v.as_str()),
            Some("My Great Song"),
            "values with spaces must be fully preserved"
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "artist")
                .map(|(_, v)| v.as_str()),
            Some("The Artist")
        );
    }
}
