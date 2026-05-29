use reqwest::Client;
use tracing::info;

const AVT_NS: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC_NS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

const AVT_PATHS: &[&str] = &[
    "/AVTransport/control",
    "/MediaRenderer/AVTransport/Control",
    "/upnp/control/AVTransport",
    "/ctl/AVTransport",
    "/upnp/control/rendertransport1",
    "/MediaRenderer_AVTransport/control",
    "/dev/AVTransport/ctrl",
    "/upnp/AVTransport/control",
    "/Control/AVTransport",
    "/dmr/AVTransport/ctrl",
];

const RC_PATHS: &[&str] = &[
    "/RenderingControl/control",
    "/MediaRenderer/RenderingControl/Control",
    "/upnp/control/RenderingControl",
    "/ctl/RenderingControl",
    "/upnp/control/rendercontrol1",
    "/MediaRenderer_RenderingControl/control",
    "/dev/RenderingControl/ctrl",
    "/upnp/RenderingControl/control",
    "/Control/RenderingControl",
    "/dmr/RenderingControl/ctrl",
];

const XML_DESC_PATHS: &[&str] = &[
    "/description.xml",
    "/DeviceDescription.xml",
    "/rootDesc.xml",
    "/dmr.xml",
];

pub struct ProbeResult {
    pub name: String,
    pub av_transport_url: String,
    pub rendering_control_url: Option<String>,
}

fn soap_body(ns: &str, action: &str, params: &[(&str, &str)]) -> String {
    let mut parts = String::new();
    for (k, v) in params {
        let escaped = quick_xml::escape::escape(*v);
        parts.push_str(&format!("<{k}>{escaped}</{k}>"));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{ns}">{parts}</u:{action}></s:Body></s:Envelope>"#
    )
}

pub async fn probe_minimal_dmr(
    base_url: &str,
    description_url: Option<&str>,
    fallback_name: &str,
) -> Option<ProbeResult> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let base = base_url.trim_end_matches('/');
    let mut name = fallback_name.to_string();

    // 1. Try XML description with generous timeout
    if let Some(desc_url) = description_url {
        if let Some(result) = try_xml_description(&client, base, desc_url, &mut name).await {
            return Some(result);
        }
    }

    // 2. Try alternative XML paths
    for xml_path in XML_DESC_PATHS {
        let alt_url = format!("{base}{xml_path}");
        if let Some(result) = try_xml_description(&client, base, &alt_url, &mut name).await {
            return Some(result);
        }
    }

    // 3. Blind-probe common AVTransport paths
    probe_common_paths(&client, base, &name).await
}

async fn try_xml_description(
    _client: &Client,
    base: &str,
    desc_url: &str,
    name: &mut String,
) -> Option<ProbeResult> {
    let generous_client = Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .ok()?;

    let resp = generous_client.get(desc_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;

    // Extract friendly name
    if let Some(start) = text.find("<friendlyName>") {
        let after = &text[start + 14..];
        if let Some(end) = after.find("</friendlyName>") {
            let friendly = after[..end].trim();
            if !friendly.is_empty() {
                *name = friendly.to_string();
            }
        }
    }

    let mut avt_url = None;
    let mut rc_url = None;

    // Parse service blocks
    let mut search_from = 0;
    while let Some(svc_start) = text[search_from..].find("<service>") {
        let abs_start = search_from + svc_start;
        let svc_end = match text[abs_start..].find("</service>") {
            Some(e) => abs_start + e + 10,
            None => break,
        };
        let block = &text[abs_start..svc_end];

        if let Some(ctrl_start) = block.find("<controlURL>") {
            let after = &block[ctrl_start + 12..];
            if let Some(ctrl_end) = after.find("</controlURL>") {
                let mut path = after[..ctrl_end].trim().to_string();
                if !path.starts_with('/') {
                    path = format!("/{path}");
                }
                let full_url = format!("{base}{path}");
                if block.contains("AVTransport:1") {
                    avt_url = Some(full_url);
                } else if block.contains("RenderingControl:1") {
                    rc_url = Some(full_url);
                }
            }
        }
        search_from = svc_end;
    }

    let avt = avt_url?;
    info!(name = %name, avt = %avt, "minimal_dmr_from_xml");
    Some(ProbeResult {
        name: name.clone(),
        av_transport_url: avt,
        rendering_control_url: rc_url,
    })
}

async fn probe_common_paths(
    client: &Client,
    base: &str,
    name: &str,
) -> Option<ProbeResult> {
    let soap = soap_body(AVT_NS, "GetTransportInfo", &[("InstanceID", "0")]);
    let soap_action = format!("{AVT_NS}#GetTransportInfo");

    let mut avt_url = None;
    for path in AVT_PATHS {
        let url = format!("{base}{path}");
        let result = client
            .post(&url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{soap_action}\""))
            .body(soap.clone())
            .send()
            .await;
        if let Ok(resp) = result {
            if resp.status().is_success() {
                info!(name, path, "minimal_dmr_avt_probed");
                avt_url = Some(url);
                break;
            }
        }
    }

    let avt = avt_url?;

    let soap_rc = soap_body(
        RC_NS,
        "GetVolume",
        &[("InstanceID", "0"), ("Channel", "Master")],
    );
    let soap_action_rc = format!("{RC_NS}#GetVolume");

    let mut rc_url = None;
    for path in RC_PATHS {
        let url = format!("{base}{path}");
        let result = client
            .post(&url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{soap_action_rc}\""))
            .body(soap_rc.clone())
            .send()
            .await;
        if let Ok(resp) = result {
            if resp.status().is_success() {
                info!(name, path, "minimal_dmr_rc_probed");
                rc_url = Some(url);
                break;
            }
        }
    }

    Some(ProbeResult {
        name: name.to_string(),
        av_transport_url: avt,
        rendering_control_url: rc_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soap_body_basic() {
        let body = soap_body(AVT_NS, "Play", &[("InstanceID", "0"), ("Speed", "1")]);
        assert!(body.contains("<InstanceID>0</InstanceID>"));
        assert!(body.contains("<Speed>1</Speed>"));
        assert!(body.contains("AVTransport:1"));
    }

    #[test]
    fn soap_body_escapes_xml() {
        let body = soap_body(AVT_NS, "SetAVTransportURI", &[("CurrentURI", "http://x.com/a&b")]);
        assert!(body.contains("&amp;b"));
    }
}
