use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::Client;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use super::didl::{DidlBuilder, ProtocolStyle};
use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SOAP_MAX_RETRIES: usize = 2;
/// Timeout for the fire-and-forget Stop sent before SetAVTransportURI.
/// Kept short (2s) because we don't need the response — SetAVTransportURI
/// implicitly stops the current track on compliant renderers.
const STOP_BEFORE_PLAY_TIMEOUT_MS: u64 = 2000;

pub struct DlnaOutput {
    name: String,
    device_id: String,
    host: String,
    av_transport_url: String,
    rendering_control_url: String,
    client: Client,
    /// Short-timeout client used for fire-and-forget Stop before play.
    stop_client: Client,
    play_delay_ms: u64,
    /// Alternates between false ("1") and true ("2") so that consecutive
    /// DIDL items sent via SetAVTransportURI / SetNextAVTransportURI use
    /// different item IDs.  Renderers like Marantz ND8006 cache DIDL
    /// metadata keyed by item id — using the same id for both current and
    /// next track causes the renderer to display stale metadata (wrong
    /// duration, format) on every other track.
    next_item_id_flip: AtomicBool,
    /// Micromega M-One uses a proprietary TCP protocol on port 7000 for volume.
    micromega_ip: Option<String>,
    /// URL for the ConnectionManager service (used to query GetProtocolInfo).
    /// Falls back to av_transport_url if not available.
    connection_manager_url: Option<String>,
}

impl DlnaOutput {
    pub fn new(
        name: String,
        device_id: String,
        host: String,
        av_transport_url: String,
        rendering_control_url: String,
        connection_manager_url: Option<String>,
    ) -> Self {
        let micromega_ip = if name.to_lowercase().contains("micromega") {
            let ip = host
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            if !ip.is_empty() {
                info!(device = %name, ip = %ip, "micromega_device_detected — proprietary volume on port 7000");
                Some(ip)
            } else {
                None
            }
        } else {
            None
        };
        Self {
            name,
            device_id,
            host,
            av_transport_url,
            rendering_control_url,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            stop_client: Client::builder()
                .timeout(std::time::Duration::from_millis(
                    STOP_BEFORE_PLAY_TIMEOUT_MS,
                ))
                .build()
                .unwrap_or_default(),
            play_delay_ms: 0,
            next_item_id_flip: AtomicBool::new(false),
            micromega_ip,
            connection_manager_url,
        }
    }

    pub fn with_play_delay(mut self, delay_ms: u64) -> Self {
        self.play_delay_ms = delay_ms;
        self
    }

    /// Send a SOAP action without retries and with the short-timeout client.
    /// Used for the fire-and-forget Stop before play — we don't need to wait
    /// for the response because SetAVTransportURI implicitly replaces the
    /// current track.  Returns immediately after the single attempt.
    async fn soap_action_fast(
        &self,
        url: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<(), String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );
        let soap_action = format!("{service}#{action}");

        match self
            .stop_client
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{soap_action}\""))
            .body(soap)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("soap_fast: {e}")),
        }
    }

    async fn soap_action(
        &self,
        url: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<String, String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );

        let soap_action = format!("{service}#{action}");
        let mut last_err = String::new();

        for attempt in 0..=SOAP_MAX_RETRIES {
            if attempt > 0 {
                let delay = 200 * (1 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                debug!(device = %self.name, action, attempt, "soap_retry");
            }

            match self
                .client
                .post(url)
                .header("Content-Type", "text/xml; charset=utf-8")
                .header("SOAPAction", format!("\"{soap_action}\""))
                .body(soap.clone())
                .send()
                .await
            {
                Ok(resp) => match resp.text().await {
                    Ok(text) => return Ok(text),
                    Err(e) => last_err = format!("soap read: {e}"),
                },
                Err(e) if e.is_connect() || e.is_timeout() => {
                    last_err = format!("soap send: {e}");
                }
                Err(e) => return Err(format!("soap send: {e}")),
            }
        }

        warn!(device = %self.name, action, error = %last_err, "soap_all_retries_failed");
        Err(last_err)
    }

    async fn av_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(&self.av_transport_url, AV_TRANSPORT_URN, action, body)
            .await
    }

    async fn rc_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(
            &self.rendering_control_url,
            RENDERING_CONTROL_URN,
            action,
            body,
        )
        .await
    }

    fn didl_metadata(media: &PlayMedia<'_>, item_id: &str) -> String {
        let is_dsd = media.mime_type.contains("dsd") || media.mime_type.contains("dsf");
        DidlBuilder::new(media.title.unwrap_or("Unknown"), media.url, media.mime_type)
            .protocol_style(ProtocolStyle::Dlna)
            .dlna_art_profile(true)
            .include_upnp_artist(true)
            .item_id(item_id)
            .artist_opt(media.artist)
            .album_opt(media.album)
            .album_art_opt(media.cover_url)
            .duration_ms_opt(media.duration_ms)
            .file_size_opt(media.file_size)
            .sample_rate_opt(if is_dsd { None } else { media.sample_rate })
            .bit_depth_opt(if is_dsd { None } else { media.bit_depth })
            .channels_opt(if is_dsd { None } else { media.channels })
            .build_escaped()
    }

    /// Return the next item id ("1" or "2") and flip the toggle.
    /// Alternating ids prevents renderers (Marantz ND8006 etc.) from
    /// displaying stale cached metadata when the same id is reused for
    /// consecutive tracks.
    fn next_item_id(&self) -> &'static str {
        let prev = self.next_item_id_flip.fetch_xor(true, Ordering::Relaxed);
        if prev { "2" } else { "1" }
    }

    fn parse_time(time_str: &str) -> u64 {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 3 {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let s_parts: Vec<&str> = parts[2].split('.').collect();
            let s: u64 = s_parts[0].parse().unwrap_or(0);
            let frac_ms: u64 = if s_parts.len() > 1 {
                let frac = s_parts[1];
                let val: u64 = frac.parse().unwrap_or(0);
                match frac.len() {
                    1 => val * 100,
                    2 => val * 10,
                    3 => val,
                    _ => val / 10u64.pow(frac.len() as u32 - 3),
                }
            } else {
                0
            };
            (h * 3600 + m * 60 + s) * 1000 + frac_ms
        } else {
            0
        }
    }

    fn format_time(ms: u64) -> String {
        let total_secs = ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

#[async_trait::async_trait]
impl OutputTarget for DlnaOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "dlna"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Fire-and-forget Stop with a tight deadline: give the renderer up to
        // 500ms to acknowledge Stop, then proceed regardless.  Most renderers
        // accept SetAVTransportURI while playing (implicit stop), but we still
        // send Stop for renderers like DMP-A8 that need it.  The short deadline
        // ensures we don't block 2-10s waiting for a slow SOAP response.
        let stop_fut = self.soap_action_fast(
            &self.av_transport_url,
            AV_TRANSPORT_URN,
            "Stop",
            "<InstanceID>0</InstanceID>",
        );
        tokio::select! {
            result = stop_fut => {
                match result {
                    Ok(()) => debug!(device = %self.name, "dlna_play_pre_stop_ok"),
                    Err(e) => debug!(device = %self.name, error = %e, "dlna_play_pre_stop_ignored"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                debug!(device = %self.name, "dlna_play_pre_stop_timeout_proceeding");
            }
        }

        let item_id = self.next_item_id();
        let metadata = Self::didl_metadata(media, item_id);
        let set_uri_resp = self.av_action("SetAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
            media.url
        )).await?;

        if set_uri_resp.contains("UPnPError") || set_uri_resp.contains("<errorCode>") {
            warn!(device = %self.name, response = %set_uri_resp, "dlna_set_uri_error");
            return Err(format!("SetAVTransportURI rejected: {set_uri_resp}"));
        }

        if self.play_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.play_delay_ms)).await;
        }

        // Retry Play with backoff — some renderers (Revox S100, stagefright-based)
        // reject Play immediately after SetAVTransportURI while still loading the URI.
        let mut last_err = String::new();
        for attempt in 0..4u32 {
            if attempt > 0 {
                let delay = 500 * (1 << (attempt - 1)); // 500ms, 1s, 2s
                info!(device = %self.name, attempt, delay_ms = delay, "dlna_play_retry");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let play_resp = self
                .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                .await?;

            if !play_resp.contains("UPnPError") && !play_resp.contains("<errorCode>") {
                if attempt > 0 {
                    info!(device = %self.name, attempt, "dlna_play_retry_succeeded");
                }
                last_err.clear();
                break;
            }
            warn!(device = %self.name, attempt, response = %play_resp, "dlna_play_error");
            last_err = format!("Play rejected: {play_resp}");
        }
        if !last_err.is_empty() {
            return Err(last_err);
        }

        info!(device = %self.name, url = media.url, delay_ms = self.play_delay_ms, "dlna_play");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.av_action("Pause", "<InstanceID>0</InstanceID>")
            .await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
            .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.av_action("Stop", "<InstanceID>0</InstanceID>").await?;
        info!(device = %self.name, "dlna_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let target = Self::format_time(position_ms);
        self.av_action(
            "Seek",
            &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>"),
        )
        .await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        if let Some(ip) = &self.micromega_ip {
            let target_vol = volume * 100.0;
            let msg = format!("GET /volume HTTP/1.0\r\n\r\nvolume={target_vol:.1}\r\n");
            let addr = format!("{ip}:7000");
            match tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(&addr))
                .await
            {
                Ok(Ok(mut stream)) => {
                    let _ = stream.write_all(msg.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    debug!(device = %self.name, volume = target_vol, "micromega_volume_set");
                }
                Ok(Err(e)) => {
                    warn!(device = %self.name, volume = target_vol, error = %e, "micromega_volume_error");
                }
                Err(_) => {
                    warn!(device = %self.name, volume = target_vol, "micromega_volume_timeout");
                }
            }
            return Ok(());
        }
        let level = (volume * 100.0).round() as u32;
        self.rc_action("SetVolume", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{level}</DesiredVolume>"
        )).await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "1" } else { "0" };
        self.rc_action("SetMute", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{val}</DesiredMute>"
        )).await?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let position_resp = self
            .av_action("GetPositionInfo", "<InstanceID>0</InstanceID>")
            .await?;
        let transport_resp = self
            .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
            .await?;
        let volume_resp = self
            .rc_action(
                "GetVolume",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await?;
        let mute_resp = self
            .rc_action(
                "GetMute",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await;

        let state = if transport_resp.contains("PLAYING") {
            TransportState::Playing
        } else if transport_resp.contains("PAUSED") {
            TransportState::Paused
        } else if transport_resp.contains("TRANSITIONING") {
            TransportState::Transitioning
        } else {
            TransportState::Stopped
        };

        let position_ms = extract_tag(&position_resp, "RelTime")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let duration_ms = extract_tag(&position_resp, "TrackDuration")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let volume = extract_tag(&volume_resp, "CurrentVolume")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);
        let muted = mute_resp
            .ok()
            .and_then(|r| extract_tag(&r, "CurrentMute"))
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let current_uri = extract_tag(&position_resp, "TrackURI");

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title: extract_tag(&position_resp, "dc:title"),
            track_artist: extract_tag(&position_resp, "dc:creator"),
            ended_naturally: false,
        })
    }

    async fn is_available(&self) -> bool {
        self.client
            .get(&self.av_transport_url)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let item_id = self.next_item_id();
        let metadata = Self::didl_metadata(media, item_id);
        self.av_action("SetNextAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{metadata}</NextURIMetaData>",
            media.url
        )).await?;
        info!(device = %self.name, url = media.url, "dlna_set_next");
        Ok(())
    }
}

impl DlnaOutput {
    pub async fn get_protocol_info(&self) -> Result<Vec<String>, String> {
        let cm_url = self
            .connection_manager_url
            .as_deref()
            .unwrap_or(&self.av_transport_url);
        let body = self
            .soap_action(
                cm_url,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                "GetProtocolInfo",
                "",
            )
            .await?;
        let sink = extract_tag(&body, "Sink").unwrap_or_default();
        Ok(sink
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DsdCapability {
    pub supports_dsf: bool,
    pub supports_dff: bool,
    pub dsf_mime: Option<String>,
}

impl DlnaOutput {
    pub async fn probe_dsd_support(&self) -> DsdCapability {
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(_) => return DsdCapability::default(),
        };
        let mut cap = DsdCapability::default();
        for proto in &protocols {
            let lower = proto.to_lowercase();
            if lower.contains("x-dsd")
                || lower.contains("audio/dsf")
                || lower.contains("application/x-dsd")
            {
                cap.supports_dsf = true;
                if cap.dsf_mime.is_none() {
                    let parts: Vec<&str> = proto.split(':').collect();
                    if parts.len() >= 3 {
                        cap.dsf_mime = Some(parts[2].trim().to_string());
                    }
                }
            }
            if lower.contains("audio/dff") || lower.contains("x-dff") {
                cap.supports_dff = true;
            }
        }
        cap
    }

    /// Probe the renderer's GetProtocolInfo Sink to check if a given MIME type
    /// is supported.  Protocol info entries have the format:
    ///   `http-get:*:audio/flac:*`
    /// The third colon-separated field is the MIME type.
    pub async fn supports_mime(&self, mime: &str) -> bool {
        let is_universal = matches!(
            mime.to_lowercase().as_str(),
            "audio/wav" | "audio/x-wav" | "audio/l16" | "audio/mpeg"
        );
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(e) => {
                // If we can't reach ConnectionManager, assume universal
                // formats (WAV/MP3) are supported but not others (FLAC).
                debug!(device = %self.name, error = %e, mime, is_universal, "protocol_info_unavailable");
                return is_universal;
            }
        };
        if protocols.is_empty() {
            debug!(device = %self.name, mime, is_universal, "protocol_info_empty_sink");
            return is_universal;
        }
        let mime_lower = mime.to_lowercase();
        for proto in &protocols {
            // Each entry: "http-get:*:audio/flac:*" or "http-get:*:audio/flac:DLNA..."
            let fields: Vec<&str> = proto.split(':').collect();
            if fields.len() >= 3 {
                let proto_mime = fields[2].trim().to_lowercase();
                if proto_mime == mime_lower {
                    return true;
                }
                // Also match wildcard MIME ("*") — some renderers advertise
                // "http-get:*:*:*" to indicate they accept anything.
                if proto_mime == "*" {
                    return true;
                }
            }
        }
        info!(device = %self.name, mime, protocols_count = protocols.len(), "dlna_mime_not_supported_by_renderer");
        false
    }
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_works() {
        assert_eq!(DlnaOutput::parse_time("0:03:45"), 225_000);
        assert_eq!(DlnaOutput::parse_time("1:00:00"), 3_600_000);
        assert_eq!(DlnaOutput::parse_time("0:00:00.000"), 0);
    }

    #[test]
    fn parse_time_fractional_seconds() {
        assert_eq!(DlnaOutput::parse_time("0:04:16.487"), 256_487);
        assert_eq!(DlnaOutput::parse_time("0:03:46.5"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.50"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.500"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:00:01.1"), 1_100);
        assert_eq!(DlnaOutput::parse_time("0:00:01.12"), 1_120);
        assert_eq!(DlnaOutput::parse_time("0:00:01.123"), 1_123);
    }

    #[test]
    fn format_time_works() {
        assert_eq!(DlnaOutput::format_time(225_000), "0:03:45");
        assert_eq!(DlnaOutput::format_time(3_600_000), "1:00:00");
    }

    #[test]
    fn extract_tag_works() {
        let xml = "<RelTime>0:03:45</RelTime><TrackDuration>0:05:30</TrackDuration>";
        assert_eq!(extract_tag(xml, "RelTime"), Some("0:03:45".into()));
        assert_eq!(extract_tag(xml, "TrackDuration"), Some("0:05:30".into()));
        assert_eq!(extract_tag(xml, "Missing"), None);
    }

    #[test]
    fn didl_metadata_with_cover_and_album() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Test Track"),
                artist: Some("Test Artist"),
                album: Some("Test Album"),
                cover_url: Some("http://example.com/cover.jpg"),
                duration_ms: Some(256_000),
                file_size: Some(50_000_000),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Test Track"));
        assert!(didl.contains("Test Artist"));
        assert!(didl.contains("Test Album"));
        assert!(didl.contains("albumArtURI"));
        assert!(didl.contains("cover.jpg"));
        assert!(
            didl.contains("dlna:profileID"),
            "albumArtURI must include dlna:profileID"
        );
        assert!(
            didl.contains("JPEG_TN"),
            "albumArtURI must use JPEG_TN profile"
        );
        assert!(
            didl.contains("xmlns:dlna"),
            "DIDL-Lite must declare xmlns:dlna namespace"
        );
        assert!(
            didl.contains("DLNA.ORG_OP=01"),
            "protocolInfo must include DLNA.ORG_OP"
        );
        assert!(
            didl.contains("DLNA.ORG_FLAGS="),
            "protocolInfo must include DLNA.ORG_FLAGS"
        );
        assert!(didl.contains("size="), "res must include size attribute");
        assert!(
            didl.contains("duration="),
            "res must include duration attribute"
        );
    }

    #[test]
    fn didl_metadata_without_cover() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Title"));
        assert!(!didl.contains("albumArtURI"));
        assert!(!didl.contains("upnp:album"));
        assert!(!didl.contains("dc:creator"));
        assert!(!didl.contains("size="));
        assert!(!didl.contains("duration="));
    }

    #[test]
    fn didl_metadata_null_artist_string() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some("null"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            !didl.contains("dc:creator"),
            "literal 'null' artist must be omitted"
        );
    }

    #[test]
    fn didl_metadata_empty_artist() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some(""),
                ..Default::default()
            },
            "1",
        );
        assert!(!didl.contains("dc:creator"), "empty artist must be omitted");
    }

    #[test]
    fn didl_escapes_special_chars() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream?a=1&b=2",
                mime_type: "audio/flac",
                title: Some("Rock & Roll"),
                artist: Some("AC/DC"),
                ..Default::default()
            },
            "1",
        );
        // build_escaped() double-escapes ampersands: first XML-escape for
        // DIDL content, then partial_escape for SOAP embedding.
        // "&" -> "&amp;" (XML) -> "&amp;amp;" (SOAP partial escape)
        // Note: quotes are NOT escaped (partial_escape), matching what
        // Denon/Marantz renderers expect in SOAP text content.
        assert!(didl.contains("Rock &amp;amp; Roll"));
        assert!(didl.contains("AC/DC"));
        assert!(didl.contains("a=1&amp;amp;b=2"));
    }

    #[test]
    fn didl_dlna_flags_wav() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/wav",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=LPCM"),
            "WAV must have LPCM profile"
        );
    }

    #[test]
    fn didl_dlna_flags_mp3() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/mpeg",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=MP3"),
            "MP3 must have MP3 profile"
        );
    }

    #[test]
    fn didl_metadata_includes_audio_params() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s.wav",
                mime_type: "audio/wav",
                title: Some("DSD Track"),
                sample_rate: Some(176_400),
                bit_depth: Some(24),
                channels: Some(2),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("sampleFrequency=\"176400\""),
            "DIDL must include sampleFrequency for DSD->PCM"
        );
        assert!(
            didl.contains("bitsPerSample=\"24\""),
            "DIDL must include bitsPerSample for DSD->PCM"
        );
        assert!(
            didl.contains("nrAudioChannels=\"2\""),
            "DIDL must include nrAudioChannels for DSD->PCM"
        );
    }

    #[test]
    fn parse_time_edge_cases() {
        assert_eq!(DlnaOutput::parse_time(""), 0);
        assert_eq!(DlnaOutput::parse_time("NOT_A_TIME"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:00"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:01"), 1_000);
        assert_eq!(DlnaOutput::parse_time("23:59:59.999"), 86_399_999);
    }

    #[test]
    fn parse_time_dmp_a6_scenario() {
        // DMP-A6 reports "0:03:46" for a track that's actually 4:16.487.
        // With fractional parsing, "0:03:46.000" should give exactly 226000ms,
        // and "0:04:16.487" should give exactly 256487ms.
        let renderer_dur = DlnaOutput::parse_time("0:03:46");
        let track_dur = DlnaOutput::parse_time("0:04:16.487");
        assert_eq!(renderer_dur, 226_000);
        assert_eq!(track_dur, 256_487);
        let diff = (track_dur as i64 - renderer_dur as i64).unsigned_abs();
        assert!(diff > 2000, "difference should exceed gapless threshold");
    }

    #[test]
    fn format_time_roundtrip() {
        for ms in [0, 1000, 60_000, 225_000, 3_600_000, 86_399_000] {
            let formatted = DlnaOutput::format_time(ms);
            let parsed = DlnaOutput::parse_time(&formatted);
            assert_eq!(parsed, ms, "roundtrip failed for {ms}ms -> {formatted}");
        }
    }

    #[test]
    fn didl_item_id_alternates() {
        // Verify that consecutive tracks get different item IDs to prevent
        // Marantz ND8006 (and similar) from displaying cached metadata.
        let didl_1 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track1",
                mime_type: "audio/flac",
                title: Some("Track 1"),
                ..Default::default()
            },
            "1",
        );
        let didl_2 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track2",
                mime_type: "audio/flac",
                title: Some("Track 2"),
                ..Default::default()
            },
            "2",
        );
        assert!(didl_1.contains("id=\"1\""), "first track should have id=1");
        assert!(didl_2.contains("id=\"2\""), "second track should have id=2");
        assert!(didl_1.contains("Track 1"));
        assert!(didl_2.contains("Track 2"));
    }
}
