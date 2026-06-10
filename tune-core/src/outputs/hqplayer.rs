use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

/// Default HQPlayer Control API port (v4/v5).
pub const HQPLAYER_DEFAULT_PORT: u16 = 4321;
/// HQPlayer v6 default control port.
pub const HQPLAYER_V6_PORT: u16 = 8019;
/// Ports to try when auto-detecting HQPlayer.
pub const HQPLAYER_PROBE_PORTS: &[u16] = &[4321, 8019];

/// XML declaration prepended to every command sent to HQPlayer.
const XML_HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

/// HQPlayer uses a custom TCP protocol with XML messages.
/// Commands are sent as XML fragments; responses are XML documents.
pub struct HqplayerOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    /// Persistent TCP connection to HQPlayer (reconnects on failure).
    connection: Arc<Mutex<Option<TcpStream>>>,
}

impl HqplayerOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            connection: Arc::new(Mutex::new(None)),
        }
    }

    /// Probe a host to find which port HQPlayer is listening on.
    /// Tries each port with a TCP connect + GetInfo handshake.
    pub async fn probe_port(host: &str) -> Option<u16> {
        for &port in HQPLAYER_PROBE_PORTS {
            match probe_hqplayer(host, port).await {
                Ok(true) => {
                    info!(host, port, "hqplayer_port_detected");
                    return Some(port);
                }
                Ok(false) => {
                    debug!(host, port, "hqplayer_port_not_hqp");
                }
                Err(e) => {
                    debug!(host, port, error = %e, "hqplayer_port_probe_failed");
                }
            }
        }
        None
    }

    /// Get or establish a TCP connection to HQPlayer.
    async fn get_connection(&self) -> Result<(), String> {
        let mut conn = self.connection.lock().await;
        if conn.is_some() {
            return Ok(());
        }
        let addr = format!("{}:{}", self.host, self.port);
        let stream =
            tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(&addr))
                .await
                .map_err(|_| format!("hqplayer connect timeout: {addr}"))?
                .map_err(|e| format!("hqplayer connect failed {addr}: {e}"))?;
        *conn = Some(stream);
        Ok(())
    }

    /// Send an XML command and receive the response.
    async fn send_command(&self, xml_body: &str) -> Result<String, String> {
        // Build full XML message
        let message = format!("{}\n{}", XML_HEADER, xml_body);

        let mut conn = self.connection.lock().await;

        // Try to use existing connection, reconnect if needed
        let stream = match conn.as_mut() {
            Some(s) => s,
            None => {
                drop(conn);
                self.get_connection().await?;
                conn = self.connection.lock().await;
                conn.as_mut()
                    .ok_or_else(|| "hqplayer: no connection after reconnect".to_string())?
            }
        };

        // Send the command
        if let Err(e) = stream.write_all(message.as_bytes()).await {
            // Connection broken, drop it and retry once
            *conn = None;
            drop(conn);
            self.get_connection().await?;
            let mut conn2 = self.connection.lock().await;
            let stream2 = conn2
                .as_mut()
                .ok_or_else(|| "hqplayer: no connection after retry".to_string())?;
            stream2
                .write_all(message.as_bytes())
                .await
                .map_err(|e2| format!("hqplayer write retry failed: {e}, then {e2}"))?;
            return read_response(stream2).await;
        }

        read_response(stream).await
    }

    /// Send a command, dropping connection on error (for next retry).
    async fn command(&self, xml_body: &str) -> Result<String, String> {
        match self.send_command(xml_body).await {
            Ok(response) => Ok(response),
            Err(e) => {
                // Drop connection so next call reconnects
                let mut conn = self.connection.lock().await;
                *conn = None;
                Err(e)
            }
        }
    }
}

/// Read XML response from HQPlayer TCP stream.
/// HQPlayer sends responses terminated by a closing XML tag.
/// We read until we get a complete XML document or timeout.
async fn read_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = vec![0u8; 8192];
    let mut response = String::new();
    let timeout = std::time::Duration::from_secs(5);

    loop {
        let n = tokio::time::timeout(timeout, stream.read(&mut buf))
            .await
            .map_err(|_| "hqplayer read timeout".to_string())?
            .map_err(|e| format!("hqplayer read error: {e}"))?;

        if n == 0 {
            return Err("hqplayer: connection closed".to_string());
        }

        response.push_str(
            std::str::from_utf8(&buf[..n]).map_err(|e| format!("hqplayer: invalid utf8: {e}"))?,
        );

        // Check if we have a complete response (ends with a closing tag)
        let trimmed = response.trim();
        if is_complete_xml(trimmed) {
            break;
        }
    }

    Ok(response)
}

/// Heuristic: XML response is complete when it ends with a closing tag whose
/// name matches the first element opened in the document.
/// Falls back to self-closing tag detection for simple responses.
fn is_complete_xml(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Find the first real element (skip <?xml ...?> processing instruction)
    let search = trimmed;
    let mut pos = 0;
    let root_tag: Option<&str> = loop {
        match search[pos..].find('<') {
            None => break None,
            Some(offset) => {
                let start = pos + offset;
                if search[start..].starts_with("<?") {
                    // Processing instruction -- skip past "?>"
                    if let Some(end) = search[start..].find("?>") {
                        pos = start + end + 2;
                        continue;
                    }
                    break None;
                }
                // Extract tag name (stops at space, /, or >)
                let after = &search[start + 1..];
                let end = after
                    .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .unwrap_or(after.len());
                if end > 0 {
                    break Some(&after[..end]);
                }
                break None;
            }
        }
    };

    match root_tag {
        Some(tag) => {
            // Check for self-closing root element
            let open_tag_prefix = format!("<{}", tag);
            if let Some(open_pos) = trimmed.find(&open_tag_prefix) {
                let after_open = &trimmed[open_pos + open_tag_prefix.len()..];
                // Find the end of this opening tag
                if let Some(gt_pos) = after_open.find('>') {
                    let before_gt = after_open[..gt_pos].trim_end();
                    if before_gt.ends_with('/') {
                        // Self-closing root: <Tag ... />
                        // Complete only if this is at the end of the document
                        let tag_end = open_pos + open_tag_prefix.len() + gt_pos + 1;
                        return trimmed[tag_end..].trim().is_empty();
                    }
                }
            }

            // Check for matching closing tag at end
            let close = format!("</{}>", tag);
            trimmed.ends_with(&close)
        }
        None => {
            // No root tag found, check generic self-closing
            trimmed.ends_with("/>")
        }
    }
}

/// Probe whether a given host:port speaks the HQPlayer control protocol.
/// Sends `<GetInfo />` and checks for valid XML response.
async fn probe_hqplayer(host: &str, port: u16) -> Result<bool, String> {
    let addr = format!("{host}:{port}");
    let mut stream =
        tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("probe timeout: {addr}"))?
            .map_err(|e| format!("probe connect: {addr}: {e}"))?;

    let cmd = format!("{}\n<GetInfo />", XML_HEADER);
    stream
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| format!("probe write: {e}"))?;

    let response = read_response(&mut stream).await?;
    // A valid HQPlayer response contains XML with version/product info
    Ok(
        response.contains("HQPlayer")
            || response.contains("hqplayer")
            || response.contains("<Info"),
    )
}

/// Parse transport state from HQPlayer XML status response.
fn parse_state_from_xml(xml: &str) -> TransportState {
    // HQPlayer status response contains state attribute or element
    let lower = xml.to_lowercase();
    if lower.contains("\"playing\"") || lower.contains(">playing<") {
        TransportState::Playing
    } else if lower.contains("\"paused\"") || lower.contains(">paused<") {
        TransportState::Paused
    } else if lower.contains("\"stopped\"") || lower.contains(">stopped<") {
        TransportState::Stopped
    } else if lower.contains("\"transitioning\"") || lower.contains("\"buffering\"") {
        TransportState::Transitioning
    } else {
        TransportState::Stopped
    }
}

/// Extract an attribute value from XML by attribute name.
fn extract_xml_attr(xml: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr_name);
    if let Some(start) = xml.find(&pattern) {
        let after = &xml[start + pattern.len()..];
        if let Some(end) = after.find('"') {
            return Some(after[..end].to_string());
        }
    }
    // Also try single quotes
    let pattern_sq = format!("{}='", attr_name);
    if let Some(start) = xml.find(&pattern_sq) {
        let after = &xml[start + pattern_sq.len()..];
        if let Some(end) = after.find('\'') {
            return Some(after[..end].to_string());
        }
    }
    None
}

/// Extract text content between XML tags: `<tag>content</tag>`.
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    if let Some(start_pos) = xml.find(&open) {
        let after_open = &xml[start_pos + open.len()..];
        // Skip to end of opening tag
        if let Some(gt) = after_open.find('>') {
            let content_start = &after_open[gt + 1..];
            if let Some(end_pos) = content_start.find(&close) {
                let text = content_start[..end_pos].trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl OutputTarget for HqplayerOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "hqplayer"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        info!(device = %self.name, url = media.url, "hqplayer_play");

        // Add URI to playlist (clear existing, start playing)
        let xml = format!(
            r#"<PlaylistAdd uri="{}" queued="0" clear="1"></PlaylistAdd>"#,
            escape_xml(media.url)
        );
        self.command(&xml).await.map_err(|e| {
            warn!(error = %e, "hqplayer_playlist_add_failed");
            e
        })?;

        // Issue play command
        self.command("<Play />").await.map_err(|e| {
            warn!(error = %e, "hqplayer_play_failed");
            e
        })?;

        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.command("<Pause />").await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.command("<Play />").await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.command("<Stop />").await?;
        info!(device = %self.name, "hqplayer_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = position_ms as f64 / 1000.0;
        let xml = format!(r#"<Seek position="{seconds:.1}" />"#);
        self.command(&xml).await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // HQPlayer volume is a dB value or a linear scale depending on config.
        // The Volume command takes a value; we pass 0-100 linear.
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        let xml = format!(r#"<Volume value="{level}" />"#);
        self.command(&xml).await?;
        Ok(())
    }

    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        // HQPlayer protocol does not have a mute toggle
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let response = self.command(r#"<Status subscribe="0" />"#).await?;

        let state = parse_state_from_xml(&response);
        let position = extract_xml_attr(&response, "position")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let duration = extract_xml_attr(&response, "duration")
            .or_else(|| extract_xml_attr(&response, "length"))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let volume = extract_xml_attr(&response, "volume")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(1.0);

        let track_title =
            extract_xml_text(&response, "Title").or_else(|| extract_xml_attr(&response, "title"));
        let track_artist =
            extract_xml_text(&response, "Artist").or_else(|| extract_xml_attr(&response, "artist"));
        let current_uri =
            extract_xml_text(&response, "Uri").or_else(|| extract_xml_attr(&response, "uri"));

        let position_ms = (position * 1000.0) as u64;
        let duration_ms = (duration * 1000.0) as u64;

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
        probe_hqplayer(&self.host, self.port).await.unwrap_or(false)
    }
}

/// Escape special XML characters in a string.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port() {
        assert_eq!(HQPLAYER_DEFAULT_PORT, 4321);
    }

    #[test]
    fn v6_port() {
        assert_eq!(HQPLAYER_V6_PORT, 8019);
    }

    #[test]
    fn output_type() {
        let hqp = HqplayerOutput::new("HQPlayer".into(), "hqp-1".into(), "localhost".into(), 4321);
        assert_eq!(hqp.output_type(), "hqplayer");
    }

    #[test]
    fn host_returned() {
        let hqp = HqplayerOutput::new(
            "HQPlayer".into(),
            "hqp-1".into(),
            "192.168.1.100".into(),
            4321,
        );
        assert_eq!(hqp.host(), Some("192.168.1.100"));
    }

    #[test]
    fn parse_state_playing() {
        let xml = r#"<Status state="playing" position="10.5" duration="300.0"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Playing);
    }

    #[test]
    fn parse_state_paused() {
        let xml = r#"<Status state="paused" position="10.5"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Paused);
    }

    #[test]
    fn parse_state_stopped() {
        let xml = r#"<Status state="stopped"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Stopped);
    }

    #[test]
    fn extract_attr() {
        let xml = r#"<Status state="playing" position="42.5" duration="180.0" volume="75"/>"#;
        assert_eq!(extract_xml_attr(xml, "position"), Some("42.5".into()));
        assert_eq!(extract_xml_attr(xml, "duration"), Some("180.0".into()));
        assert_eq!(extract_xml_attr(xml, "volume"), Some("75".into()));
        assert_eq!(extract_xml_attr(xml, "missing"), None);
    }

    #[test]
    fn extract_text() {
        let xml = r#"<Status><Title>Test Song</Title><Artist>Test Artist</Artist></Status>"#;
        assert_eq!(extract_xml_text(xml, "Title"), Some("Test Song".into()));
        assert_eq!(extract_xml_text(xml, "Artist"), Some("Test Artist".into()));
    }

    #[test]
    fn escape_xml_chars() {
        assert_eq!(
            escape_xml(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn is_complete_self_closing() {
        assert!(is_complete_xml(r#"<Status state="stopped" />"#));
        assert!(is_complete_xml(
            r#"<?xml version="1.0"?><Info name="HQPlayer"/>"#
        ));
    }

    #[test]
    fn is_complete_closing_tag() {
        assert!(is_complete_xml("<Status><Title>x</Title></Status>"));
        assert!(is_complete_xml("<LibraryGet></LibraryGet>"));
    }

    #[test]
    fn is_not_complete() {
        assert!(!is_complete_xml("<Status"));
        assert!(!is_complete_xml(""));
        assert!(!is_complete_xml("<Status><Title>x</Title>"));
    }
}
