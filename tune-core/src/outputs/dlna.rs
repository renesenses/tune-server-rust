use reqwest::Client;
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, TransportState};

const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

pub struct DlnaOutput {
    name: String,
    device_id: String,
    host: String,
    av_transport_url: String,
    rendering_control_url: String,
    client: Client,
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
                .unwrap(),
        }
    }

    async fn soap_action(&self, url: &str, service: &str, action: &str, body: &str) -> Result<String, String> {
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

        let resp = self.client
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{soap_action}\""))
            .body(soap)
            .send()
            .await
            .map_err(|e| format!("soap send: {e}"))?;

        resp.text().await.map_err(|e| format!("soap read: {e}"))
    }

    async fn av_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(&self.av_transport_url, AV_TRANSPORT_URN, action, body).await
    }

    async fn rc_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(&self.rendering_control_url, RENDERING_CONTROL_URN, action, body).await
    }

    fn didl_metadata(title: Option<&str>, artist: Option<&str>, mime_type: &str, url: &str) -> String {
        let title = title.unwrap_or("Unknown");
        let artist = artist.unwrap_or("Unknown");
        let escaped_url = url.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        format!(
            r#"&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;&lt;item id="1" parentID="0" restricted="1"&gt;&lt;dc:title&gt;{title}&lt;/dc:title&gt;&lt;dc:creator&gt;{artist}&lt;/dc:creator&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;&lt;res protocolInfo="http-get:*:{mime_type}:*"&gt;{escaped_url}&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;"#
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

    async fn play_url(&self, url: &str, mime_type: &str, title: Option<&str>, artist: Option<&str>) -> Result<(), String> {
        let metadata = Self::didl_metadata(title, artist, mime_type, url);
        self.av_action("SetAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>"
        )).await?;

        self.av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>").await?;
        info!(device = %self.name, url, "dlna_play");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.av_action("Pause", "<InstanceID>0</InstanceID>").await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>").await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.av_action("Stop", "<InstanceID>0</InstanceID>").await?;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let target = Self::format_time(position_ms);
        self.av_action("Seek", &format!(
            "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>"
        )).await?;
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
        let position_resp = self.av_action("GetPositionInfo", "<InstanceID>0</InstanceID>").await?;
        let transport_resp = self.av_action("GetTransportInfo", "<InstanceID>0</InstanceID>").await?;
        let volume_resp = self.rc_action("GetVolume", "<InstanceID>0</InstanceID><Channel>Master</Channel>").await?;

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
        let current_uri = extract_tag(&position_resp, "TrackURI");

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted: false,
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
}
