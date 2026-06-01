use reqwest::Client;
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SOAP_MAX_RETRIES: usize = 2;

pub struct DlnaOutput {
    name: String,
    device_id: String,
    av_transport_url: String,
    rendering_control_url: String,
    client: Client,
    play_delay_ms: u64,
}

impl DlnaOutput {
    pub fn new(
        name: String,
        device_id: String,
        _host: String,
        av_transport_url: String,
        rendering_control_url: String,
    ) -> Self {
        Self {
            name,
            device_id,
            av_transport_url,
            rendering_control_url,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
            play_delay_ms: 0,
        }
    }

    pub fn with_play_delay(mut self, delay_ms: u64) -> Self {
        self.play_delay_ms = delay_ms;
        self
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

    /// Return true when the value is a usable metadata string (not empty,
    /// not the literal `"null"` that JavaScript clients sometimes send).
    fn is_valid_meta(v: Option<&str>) -> bool {
        matches!(v, Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    }

    fn didl_metadata(
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        mime_type: &str,
        url: &str,
        cover_url: Option<&str>,
    ) -> String {
        let title = quick_xml::escape::escape(
            if Self::is_valid_meta(title) { title.unwrap() } else { "Unknown" },
        );
        let escaped_url = quick_xml::escape::escape(url);

        let artist_tag = if Self::is_valid_meta(artist) {
            let a = quick_xml::escape::escape(artist.unwrap());
            format!("&lt;dc:creator&gt;{a}&lt;/dc:creator&gt;")
        } else {
            String::new()
        };

        let album_tag = album
            .filter(|a| Self::is_valid_meta(Some(a)))
            .map(|a| {
                let a = quick_xml::escape::escape(a);
                format!("&lt;upnp:album&gt;{a}&lt;/upnp:album&gt;")
            })
            .unwrap_or_default();

        let art_tag = cover_url
            .map(|c| {
                let c = quick_xml::escape::escape(c);
                format!("&lt;upnp:albumArtURI&gt;{c}&lt;/upnp:albumArtURI&gt;")
            })
            .unwrap_or_default();

        format!(
            r#"&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;&lt;item id="1" parentID="0" restricted="1"&gt;&lt;dc:title&gt;{title}&lt;/dc:title&gt;{artist_tag}&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;{album_tag}{art_tag}&lt;res protocolInfo="http-get:*:{mime_type}:*"&gt;{escaped_url}&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;"#
        )
    }

    fn parse_time(time_str: &str) -> u64 {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 3 {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let s_parts: Vec<&str> = parts[2].split('.').collect();
            let s: u64 = s_parts[0].parse().unwrap_or(0);
            (h * 3600 + m * 60 + s) * 1000
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

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let stop_resp = self.av_action("Stop", "<InstanceID>0</InstanceID>").await;
        match &stop_resp {
            Ok(_) => debug!(device = %self.name, "dlna_play_pre_stop_ok"),
            Err(e) => debug!(device = %self.name, error = %e, "dlna_play_pre_stop_ignored"),
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let metadata = Self::didl_metadata(
            media.title,
            media.artist,
            media.album,
            media.mime_type,
            media.url,
            media.cover_url,
        );
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
        );
        self.av_action("SetNextAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{metadata}</NextURIMetaData>",
            media.url
        )).await?;
        info!(device = %self.name, url = media.url, "dlna_set_next");
        Ok(())
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
        );
        assert!(didl.contains("Test Track"));
        assert!(didl.contains("Test Artist"));
        assert!(didl.contains("Test Album"));
        assert!(didl.contains("albumArtURI"));
        assert!(didl.contains("cover.jpg"));
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
        );
        assert!(didl.contains("Title"));
        assert!(!didl.contains("albumArtURI"));
        assert!(!didl.contains("upnp:album"));
        // artist tag must be omitted when None
        assert!(!didl.contains("dc:creator"));
    }

    #[test]
    fn didl_metadata_null_artist_string() {
        // JavaScript clients may send the literal string "null"
        let didl = DlnaOutput::didl_metadata(
            Some("Title"),
            Some("null"),
            None,
            "audio/flac",
            "http://example.com/stream",
            None,
        );
        assert!(!didl.contains("dc:creator"), "literal 'null' artist must be omitted");
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
        );
        assert!(didl.contains("Rock &amp; Roll"));
        assert!(didl.contains("AC/DC"));
        assert!(didl.contains("a=1&amp;b=2"));
    }
}
