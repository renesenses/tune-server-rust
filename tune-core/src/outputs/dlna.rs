use reqwest::Client;
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
}

impl DlnaOutput {
    pub fn new(
        name: String,
        device_id: String,
        host: String,
        av_transport_url: String,
        rendering_control_url: String,
    ) -> Self {
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

    fn didl_metadata(
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        mime_type: &str,
        url: &str,
        cover_url: Option<&str>,
        duration_ms: Option<u64>,
        file_size: Option<u64>,
    ) -> String {
        DidlBuilder::new(title.unwrap_or("Unknown"), url, mime_type)
            .protocol_style(ProtocolStyle::Dlna)
            .dlna_art_profile(true)
            .item_id("1")
            .artist_opt(artist)
            .album_opt(album)
            .album_art_opt(cover_url)
            .duration_ms_opt(duration_ms)
            .file_size_opt(file_size)
            .build_escaped()
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

        let metadata = Self::didl_metadata(
            media.title,
            media.artist,
            media.album,
            media.mime_type,
            media.url,
            media.cover_url,
            media.duration_ms,
            media.file_size,
        );
        let set_uri_resp = self.av_action("SetAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
            media.url
        )).await?;

        if set_uri_resp.contains("UPnPError") || set_uri_resp.contains("<errorCode>") {
            warn!(device = %self.name, response = %set_uri_resp, "dlna_set_uri_error");
            return Err(format!("SetAVTransportURI rejected: {set_uri_resp}"));
        }

        if let Some(ref notify) = media.stream_ready {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified()).await;
            debug!(device = %self.name, "dlna_stream_ready_received");
        } else if self.play_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.play_delay_ms)).await;
        }

        let play_resp = self
            .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
            .await?;

        if play_resp.contains("UPnPError") || play_resp.contains("<errorCode>") {
            warn!(device = %self.name, response = %play_resp, "dlna_play_error");
            return Err(format!("Play rejected: {play_resp}"));
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
        let metadata = Self::didl_metadata(
            media.title,
            media.artist,
            media.album,
            media.mime_type,
            media.url,
            media.cover_url,
            media.duration_ms,
            media.file_size,
        );
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
        let body = self
            .soap_action(
                &self.av_transport_url,
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
            Some("Test Track"),
            Some("Test Artist"),
            Some("Test Album"),
            "audio/flac",
            "http://example.com/stream",
            Some("http://example.com/cover.jpg"),
            Some(256_000),
            Some(50_000_000),
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
            Some("Title"),
            None,
            None,
            "audio/flac",
            "http://example.com/stream",
            None,
            None,
            None,
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
            Some("Title"),
            Some("null"),
            None,
            "audio/flac",
            "http://example.com/stream",
            None,
            None,
            None,
        );
        assert!(
            !didl.contains("dc:creator"),
            "literal 'null' artist must be omitted"
        );
    }

    #[test]
    fn didl_metadata_empty_artist() {
        let didl = DlnaOutput::didl_metadata(
            Some("Title"),
            Some(""),
            None,
            "audio/flac",
            "http://example.com/stream",
            None,
            None,
            None,
        );
        assert!(!didl.contains("dc:creator"), "empty artist must be omitted");
    }

    #[test]
    fn didl_escapes_special_chars() {
        let didl = DlnaOutput::didl_metadata(
            Some("Rock & Roll"),
            Some("AC/DC"),
            None,
            "audio/flac",
            "http://example.com/stream?a=1&b=2",
            None,
            None,
            None,
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
            Some("T"),
            None,
            None,
            "audio/wav",
            "http://x/s",
            None,
            None,
            None,
        );
        assert!(
            didl.contains("DLNA.ORG_PN=LPCM"),
            "WAV must have LPCM profile"
        );
    }

    #[test]
    fn didl_dlna_flags_mp3() {
        let didl = DlnaOutput::didl_metadata(
            Some("T"),
            None,
            None,
            "audio/mpeg",
            "http://x/s",
            None,
            None,
            None,
        );
        assert!(
            didl.contains("DLNA.ORG_PN=MP3"),
            "MP3 must have MP3 profile"
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
}
