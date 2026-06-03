use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use tracing::debug;

#[derive(Debug, Clone, Default)]
pub struct DeviceDescription {
    pub friendly_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub model_description: String,
    pub udn: String,
    pub device_type: String,
    pub services: Vec<ServiceDescription>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceDescription {
    pub service_type: String,
    pub service_id: String,
    pub control_url: String,
    pub event_sub_url: String,
    pub scpd_url: String,
}

impl DeviceDescription {
    pub fn is_media_renderer(&self) -> bool {
        self.device_type.contains("MediaRenderer")
    }

    pub fn is_media_server(&self) -> bool {
        self.device_type.contains("MediaServer")
    }

    pub fn is_openhome(&self) -> bool {
        self.services
            .iter()
            .any(|s| s.service_type.contains("av-openhome-org"))
    }

    /// Returns true if the device exposes an AVTransport service, regardless of deviceType.
    /// This catches renderers (WiiM, foobar2000 foo_upnp, etc.) that use non-standard
    /// device types but still support DLNA playback via AVTransport.
    pub fn has_av_transport(&self) -> bool {
        self.services
            .iter()
            .any(|s| s.service_type.contains("AVTransport"))
    }

    pub fn service_urls(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for svc in &self.services {
            let key = service_key(&svc.service_type);
            map.insert(key, svc.control_url.clone());
        }
        map
    }

    pub fn event_sub_urls(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for svc in &self.services {
            let key = service_key(&svc.service_type);
            map.insert(key, svc.event_sub_url.clone());
        }
        map
    }
}

fn service_key(service_type: &str) -> String {
    let lower = service_type.to_lowercase();
    for name in [
        "avtransport",
        "renderingcontrol",
        "connectionmanager",
        "contentdirectory",
        "product",
        "playlist",
        "transport",
        "volume",
        "info",
        "time",
        "pins",
    ] {
        if lower.contains(name) {
            return name.to_string();
        }
    }
    lower
}

pub fn parse_device_description(xml: &str) -> Result<DeviceDescription, String> {
    let mut reader = Reader::from_str(xml);
    let mut desc = DeviceDescription::default();
    let mut current_service = ServiceDescription::default();
    let mut in_service = false;
    let mut current_tag = String::new();
    let mut buf = Vec::new();
    let mut depth_in_device = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                current_tag = tag.clone();
                match tag.as_str() {
                    "device" => depth_in_device += 1,
                    "service" => {
                        in_service = true;
                        current_service = ServiceDescription::default();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match tag.as_str() {
                    "device" => depth_in_device -= 1,
                    "service" => {
                        in_service = false;
                        if !current_service.service_type.is_empty() {
                            desc.services.push(current_service.clone());
                        }
                    }
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().unwrap_or_default().trim().to_string();
                if text.is_empty() {
                    continue;
                }
                if in_service {
                    match current_tag.as_str() {
                        "serviceType" => current_service.service_type = text,
                        "serviceId" => current_service.service_id = text,
                        "controlURL" => current_service.control_url = text,
                        "eventSubURL" => current_service.event_sub_url = text,
                        "SCPDURL" => current_service.scpd_url = text,
                        _ => {}
                    }
                } else if depth_in_device > 0 {
                    match current_tag.as_str() {
                        "friendlyName" => desc.friendly_name = text,
                        "manufacturer" => desc.manufacturer = text,
                        "modelName" => desc.model_name = text,
                        "modelDescription" => desc.model_description = text,
                        "UDN" => desc.udn = text,
                        "deviceType" => desc.device_type = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                debug!(error = %e, "xml_parse_error");
                return Err(format!("XML parse error: {e}"));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(desc)
}

pub async fn fetch_device_description(location: &str) -> Result<DeviceDescription, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let xml = client
        .get(location)
        .send()
        .await
        .map_err(|e| format!("HTTP fetch {location}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("HTTP body {location}: {e}"))?;

    parse_device_description(&xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Living Room Speaker</friendlyName>
    <manufacturer>Denon</manufacturer>
    <modelName>DMP-A8</modelName>
    <modelDescription>Network Audio Player</modelDescription>
    <UDN>uuid:12345678-1234-1234-1234-123456789abc</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/MediaRenderer/AVTransport/Control</controlURL>
        <eventSubURL>/MediaRenderer/AVTransport/Event</eventSubURL>
        <SCPDURL>/MediaRenderer/AVTransport/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/MediaRenderer/RenderingControl/Control</controlURL>
        <eventSubURL>/MediaRenderer/RenderingControl/Event</eventSubURL>
        <SCPDURL>/MediaRenderer/RenderingControl/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parse_media_renderer() {
        let desc = parse_device_description(SAMPLE_XML).unwrap();
        assert_eq!(desc.friendly_name, "Living Room Speaker");
        assert_eq!(desc.manufacturer, "Denon");
        assert_eq!(desc.model_name, "DMP-A8");
        assert!(desc.is_media_renderer());
        assert!(!desc.is_openhome());
        assert_eq!(desc.services.len(), 2);
        let urls = desc.service_urls();
        assert!(urls.contains_key("avtransport"));
        assert!(urls.contains_key("renderingcontrol"));
    }

    const OPENHOME_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Linn Klimax DSM</friendlyName>
    <manufacturer>Linn</manufacturer>
    <UDN>uuid:linn-1</UDN>
    <serviceList>
      <service>
        <serviceType>urn:av-openhome-org:service:Product:1</serviceType>
        <serviceId>urn:av-openhome-org:serviceId:Product</serviceId>
        <controlURL>/product/control</controlURL>
        <eventSubURL>/product/event</eventSubURL>
        <SCPDURL>/product/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:av-openhome-org:service:Playlist:1</serviceType>
        <serviceId>urn:av-openhome-org:serviceId:Playlist</serviceId>
        <controlURL>/playlist/control</controlURL>
        <eventSubURL>/playlist/event</eventSubURL>
        <SCPDURL>/playlist/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parse_openhome_device() {
        let desc = parse_device_description(OPENHOME_XML).unwrap();
        assert!(desc.is_openhome());
        assert_eq!(desc.friendly_name, "Linn Klimax DSM");
        let urls = desc.service_urls();
        assert!(urls.contains_key("product"));
        assert!(urls.contains_key("playlist"));
    }

    /// WiiM devices may advertise as PlayGroupManager instead of MediaRenderer,
    /// but they still expose AVTransport and should be discovered as DLNA renderers.
    const WIIM_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-wiimu-com:device:PlayGroupManager:1</deviceType>
    <friendlyName>WiiM Pro</friendlyName>
    <manufacturer>Linkplay Technology Inc.</manufacturer>
    <modelName>WiiM Pro</modelName>
    <UDN>uuid:wiim-1234</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/upnp/control/AVTransport</controlURL>
        <eventSubURL>/upnp/event/AVTransport</eventSubURL>
        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/upnp/control/RenderingControl</controlURL>
        <eventSubURL>/upnp/event/RenderingControl</eventSubURL>
        <SCPDURL>/RenderingControl/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parse_wiim_non_standard_device_type() {
        let desc = parse_device_description(WIIM_XML).unwrap();
        assert_eq!(desc.friendly_name, "WiiM Pro");
        assert_eq!(desc.manufacturer, "Linkplay Technology Inc.");
        // Not a standard MediaRenderer deviceType
        assert!(!desc.is_media_renderer());
        // But has AVTransport => should be accepted as DLNA renderer
        assert!(desc.has_av_transport());
        assert!(!desc.is_openhome());
        let urls = desc.service_urls();
        assert!(urls.contains_key("avtransport"));
        assert!(urls.contains_key("renderingcontrol"));
    }

    /// foobar2000 with foo_upnp may advertise with a non-standard device type
    /// but still support AVTransport for DLNA playback.
    const FOOBAR_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType>
    <friendlyName>foobar2000</friendlyName>
    <manufacturer>Peter Pawlowski</manufacturer>
    <modelName>foobar2000</modelName>
    <UDN>uuid:foobar-5678</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/ctrl/AVTransport</controlURL>
        <eventSubURL>/evt/AVTransport</eventSubURL>
        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parse_foobar_basic_device_with_avtransport() {
        let desc = parse_device_description(FOOBAR_XML).unwrap();
        assert_eq!(desc.friendly_name, "foobar2000");
        assert!(!desc.is_media_renderer());
        assert!(desc.has_av_transport());
        let urls = desc.service_urls();
        assert!(urls.contains_key("avtransport"));
    }

    /// A pure media server without AVTransport should NOT be accepted.
    const PURE_SERVER_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>MinimServer</friendlyName>
    <manufacturer>MinimServer</manufacturer>
    <UDN>uuid:ms-1</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <controlURL>/ctrl/ContentDirectory</controlURL>
        <eventSubURL>/evt/ContentDirectory</eventSubURL>
        <SCPDURL>/ContentDirectory/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn pure_media_server_has_no_avtransport() {
        let desc = parse_device_description(PURE_SERVER_XML).unwrap();
        assert!(!desc.is_media_renderer());
        assert!(!desc.has_av_transport());
        assert!(desc.is_media_server());
    }
}
