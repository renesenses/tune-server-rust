use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info};

use super::traits::*;

/// LMS CLI port (telnet-style protocol). NOT 9000 (JSON-RPC/HTTP).
pub const LMS_CLI_PORT: u16 = 9090;

/// Per-read socket timeout for the LMS CLI. A busy LMS can take a moment to
/// answer, so a single window used to be too tight (see [`CLI_READ_DEADLINE`]).
const CLI_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Overall deadline for assembling one CLI response line. The socket read
/// timeout ([`CLI_READ_TIMEOUT`]) can surface a transient `EAGAIN`
/// (`WouldBlock`, os error 11) when LMS is slow to send the next chunk; we
/// retry within this window instead of aborting — a single EAGAIN used to be
/// treated as fatal and immediately stop the zone (Yacine, Freebox Delta).
const CLI_READ_DEADLINE: Duration = Duration::from_secs(12);

/// Read one newline-terminated line, tolerating transient
/// `WouldBlock`/`TimedOut`/`Interrupted` (EAGAIN / EINTR) errors from the
/// socket read timeout by retrying until `deadline`, rather than aborting the
/// whole command. Partial bytes read before a transient error are preserved
/// across retries (`read_line` appends), so the assembled line is complete.
fn read_line_tolerant<R: BufRead>(reader: &mut R, deadline: Instant) -> Result<String, String> {
    let mut response = String::new();
    loop {
        match reader.read_line(&mut response) {
            // Ok(0) = EOF, Ok(n) = got a line (or EOF-terminated last line).
            Ok(_) => return Ok(response),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(format!("LMS CLI read timed out: {e}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("LMS CLI read failed: {e}")),
        }
    }
}

pub struct SqueezeboxOutput {
    name: String,
    device_id: String,
    player_id: String,
    lms_host: String,
    lms_port: u16,
    muted: AtomicBool,
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
            muted: AtomicBool::new(false),
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
            .set_read_timeout(Some(CLI_READ_TIMEOUT))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(CLI_READ_TIMEOUT))
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
        let response = read_line_tolerant(&mut reader, Instant::now() + CLI_READ_DEADLINE)?;

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

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false).with_percent_volume()
    }

    /// Opt out of the poller's position-polling (DLNA-style) gapless. On this
    /// LMS-CLI proxy, staging the next track is `playlist add` (an append to
    /// LMS's OWN playlist), and the poller's gapless advance is metadata-only —
    /// it never re-issues a play. So LMS ends up with an independent, growing
    /// playlist that Tune no longer commands track-by-track: after end-of-track
    /// LMS free-runs its own playlist while Tune silently updates now-playing
    /// (Yacine, zone 7 — repeat=one re-appends the same track, then a random LMS
    /// track plays with no Tune trace). With gapless off, the poller's
    /// natural-end path issues an explicit `playlist play <next>` per track,
    /// keeping Tune in control (and looping a 1-track Repeat queue). Same choice
    /// as the native slimproto output; small inter-track gap is the accepted
    /// trade-off for a CLI-driven renderer.
    fn supports_internal_gapless(&self) -> bool {
        false
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
        self.muted.store(muted, Ordering::Relaxed);
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
            muted: self.muted.load(Ordering::Relaxed),
            current_uri,
            track_title,
            track_artist,
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
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

    // A `Read` that replays a scripted sequence of results, so we can drive
    // `read_line_tolerant` through transient errors deterministically.
    struct FlakyReader {
        steps: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }
    impl std::io::Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                Some(Ok(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Ok(0), // exhausted → EOF
            }
        }
    }

    fn flaky(steps: Vec<std::io::Result<Vec<u8>>>) -> BufReader<FlakyReader> {
        BufReader::new(FlakyReader {
            steps: steps.into(),
        })
    }

    #[test]
    fn read_line_tolerant_retries_transient_then_succeeds() {
        // Two EAGAIN (WouldBlock, os error 11) then the real line → must succeed.
        let mut r = flaky(vec![
            Err(std::io::Error::from(ErrorKind::WouldBlock)),
            Err(std::io::Error::from(ErrorKind::WouldBlock)),
            Ok(b"player status ok\n".to_vec()),
        ]);
        let line = read_line_tolerant(&mut r, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(line, "player status ok\n");
    }

    #[test]
    fn read_line_tolerant_preserves_partial_across_retries() {
        // A chunk without a newline, then WouldBlock, then the remainder: the
        // reassembled line must contain both halves.
        let mut r = flaky(vec![
            Ok(b"abc ".to_vec()),
            Err(std::io::Error::from(ErrorKind::WouldBlock)),
            Ok(b"def\n".to_vec()),
        ]);
        let line = read_line_tolerant(&mut r, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(line, "abc def\n");
    }

    #[test]
    fn read_line_tolerant_times_out_when_deadline_passed() {
        // Deadline already in the past → the first WouldBlock is fatal.
        let mut r = flaky(vec![Err(std::io::Error::from(ErrorKind::WouldBlock))]);
        let err = read_line_tolerant(&mut r, Instant::now() - Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[test]
    fn read_line_tolerant_propagates_hard_error() {
        // A non-transient error is returned immediately, not retried.
        let mut r = flaky(vec![Err(std::io::Error::from(ErrorKind::ConnectionReset))]);
        let err = read_line_tolerant(&mut r, Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("read failed"), "got: {err}");
    }

    #[test]
    fn read_line_tolerant_eof_returns_available() {
        // EOF with a prior partial (no newline) → return what we have.
        let mut r = flaky(vec![Ok(b"partial".to_vec())]);
        let line = read_line_tolerant(&mut r, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(line, "partial");
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
