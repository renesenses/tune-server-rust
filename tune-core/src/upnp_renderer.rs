//! MediaRenderer:1 UPnP devant les zones (#1750).
//!
//! Le MediaServer (`upnp_server.rs`) expose la bibliothèque ; ce module fait
//! l'inverse : chaque zone OPT-IN (`zone_{id}_upnp_renderer` = "true")
//! s'annonce comme un renderer que JPlay/BubbleUPnP/mconnect peuvent piloter.
//! Le flux reçu (SetAVTransportURI) traverse toute la chaîne Tune — EQ,
//! convolveur, trim, multiroom — via le chemin `source = "upnp"` existant.
//!
//! Ce fichier est PUR (parsing SOAP → commandes, réponses, SSDP) : aucune
//! exécution. La couche route (`tune-server/src/routes/upnp_media_renderer.rs`)
//! traduit les commandes en appels orchestrateur.

use quick_xml::escape::unescape;
use quick_xml::events::Event;

/// Préfixe de montage des renderers, sous le routeur UPnP existant.
pub const RENDERER_MOUNT: &str = "/upnp/renderer";

const AVTRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERINGCONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

// ---------------------------------------------------------------------------
// Description & SCPD
// ---------------------------------------------------------------------------

/// Description XML d'un renderer de zone. Toutes les URLs sont absolues et
/// portent le préfixe de montage — la leçon du MediaServer (#1613/#1719).
pub fn renderer_description_xml(
    friendly_name: &str,
    uuid: &str,
    base_url: &str,
    zone_id: i64,
) -> String {
    let base = format!("{base_url}{RENDERER_MOUNT}/{zone_id}");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>
    <friendlyName>{friendly}</friendlyName>
    <manufacturer>MozAIk Labs</manufacturer>
    <manufacturerURL>https://mozaiklabs.fr</manufacturerURL>
    <modelDescription>Tune Zone Renderer</modelDescription>
    <modelName>Tune</modelName>
    <modelNumber>{version}</modelNumber>
    <modelURL>https://mozaiklabs.fr/tune</modelURL>
    <serialNumber>{version}</serialNumber>
    <UDN>{uuid}</UDN>
    <serviceList>
      <service>
        <serviceType>{av}</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>{base}/AVTransport/control</controlURL>
        <eventSubURL>{base}/AVTransport/event</eventSubURL>
        <SCPDURL>{base}/AVTransport/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>{rc}</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>{base}/RenderingControl/control</controlURL>
        <eventSubURL>{base}/RenderingControl/event</eventSubURL>
        <SCPDURL>{base}/RenderingControl/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <controlURL>{base}/ConnectionManager/control</controlURL>
        <eventSubURL>{base}/ConnectionManager/event</eventSubURL>
        <SCPDURL>{base}/ConnectionManager/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        friendly = quick_xml::escape::escape(friendly_name),
        version = crate::version(),
        uuid = uuid,
        base = base,
        av = AVTRANSPORT_URN,
        rc = RENDERINGCONTROL_URN,
    )
}

/// SCPD AVTransport — les actions que JPlay & co appellent réellement.
pub fn avtransport_scpd() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>SetAVTransportURI</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
      <argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>SetNextAVTransportURI</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>NextURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
      <argument><name>NextURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Play</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Pause</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Stop</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Seek</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
      <argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetTransportInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
      <argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
      <argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetPositionInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
      <argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
      <argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
      <argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
      <argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
      <argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
      <argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
      <argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetMediaInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>NrTracks</name><direction>out</direction><relatedStateVariable>NumberOfTracks</relatedStateVariable></argument>
      <argument><name>MediaDuration</name><direction>out</direction><relatedStateVariable>CurrentMediaDuration</relatedStateVariable></argument>
      <argument><name>CurrentURI</name><direction>out</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
      <argument><name>CurrentURIMetaData</name><direction>out</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
    </argumentList></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType>
      <allowedValueList><allowedValue>REL_TIME</allowedValue><allowedValue>TRACK_NR</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>TransportState</name><dataType>string</dataType>
      <allowedValueList><allowedValue>STOPPED</allowedValue><allowedValue>PLAYING</allowedValue><allowedValue>PAUSED_PLAYBACK</allowedValue><allowedValue>TRANSITIONING</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>TransportStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrack</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NumberOfTracks</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentMediaDuration</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

/// SCPD RenderingControl — volume et mute, canal Master.
pub fn renderingcontrol_scpd() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>GetVolume</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>SetVolume</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetMute</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>SetMute</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
    </argumentList></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType>
      <allowedValueList><allowedValue>Master</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="yes"><name>Volume</name><dataType>ui2</dataType>
      <allowedValueRange><minimum>0</minimum><maximum>100</maximum></allowedValueRange>
    </stateVariable>
    <stateVariable sendEvents="yes"><name>Mute</name><dataType>boolean</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

// ---------------------------------------------------------------------------
// Commandes SOAP
// ---------------------------------------------------------------------------

/// Une action AVTransport/RenderingControl parsée — la couche route exécute.
#[derive(Debug, Clone, PartialEq)]
pub enum RendererCommand {
    SetUri {
        uri: String,
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
    },
    /// SetNextAVTransportURI : la piste à enchaîner quand la courante finit.
    SetNextUri {
        uri: String,
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
    },
    Play,
    Pause,
    Stop,
    /// Position absolue demandée (REL_TIME), en millisecondes.
    Seek(u64),
    GetTransportInfo,
    GetPositionInfo,
    GetMediaInfo,
    GetVolume,
    SetVolume(u8),
    GetMute,
    SetMute(bool),
    /// Action non prise en charge → fault 401 (nom conservé pour le log).
    Unsupported(String),
}

/// Extrait le texte du premier élément nommé `tag` (sans préfixe) du corps.
///
/// quick_xml ≥ 0.37 découpe les entités (`&lt;` …) en événements `GeneralRef`
/// séparés : un CurrentURIMetaData — du DIDL intégralement échappé — arrive en
/// une ALTERNANCE de Text et de GeneralRef. On accumule jusqu'à la fermeture
/// de l'élément visé, sinon on ne lirait que le premier fragment.
fn text_of(soap_xml: &str, tag: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(soap_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut inside = false;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                inside = name.rsplit(':').next().unwrap_or(&name) == tag;
                if inside {
                    acc.clear();
                }
            }
            Ok(Event::Text(t)) if inside => {
                let decoded = t.decode().unwrap_or_default();
                match unescape(&decoded) {
                    Ok(c) => acc.push_str(&c),
                    Err(_) => acc.push_str(&decoded),
                }
            }
            Ok(Event::GeneralRef(r)) if inside => {
                let name = String::from_utf8_lossy(r.as_ref()).to_string();
                match name.as_str() {
                    "lt" => acc.push('<'),
                    "gt" => acc.push('>'),
                    "amp" => acc.push('&'),
                    "quot" => acc.push('"'),
                    "apos" => acc.push('\''),
                    n if n.starts_with('#') => {
                        let code = n.trim_start_matches('#');
                        let v = if let Some(hex) =
                            code.strip_prefix('x').or_else(|| code.strip_prefix('X'))
                        {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            code.parse::<u32>().ok()
                        };
                        if let Some(c) = v.and_then(char::from_u32) {
                            acc.push(c);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                if inside {
                    return Some(acc);
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// `H:MM:SS[.mmm]` → millisecondes. Tolère `HH:MM:SS` et les fractions.
pub fn parse_upnp_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b.parse::<u64>().ok().map(|_| b)),
        None => (s, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, sec): (u64, u64, u64) = match parts.as_slice() {
        [h, m, s] => (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?),
        [m, s] => (0, m.parse().ok()?, s.parse().ok()?),
        _ => return None,
    };
    let mut ms = (h * 3600 + m * 60 + sec) * 1000;
    if let Some(f) = frac {
        let scaled: u64 = f.parse().unwrap_or(0);
        ms += match f.len() {
            1 => scaled * 100,
            2 => scaled * 10,
            _ => scaled.min(999),
        };
    }
    Some(ms)
}

/// Millisecondes → `H:MM:SS` (format UPnP).
pub fn format_upnp_time(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// Métadonnées utiles d'un fragment DIDL-Lite (CurrentURIMetaData).
fn parse_didl_metadata(didl: &str) -> (Option<String>, Option<String>, Option<i64>) {
    if didl.trim().is_empty() {
        return (None, None, None);
    }
    let title = text_of(didl, "title").filter(|s| !s.is_empty());
    let artist = text_of(didl, "artist")
        .or_else(|| text_of(didl, "creator"))
        .filter(|s| !s.is_empty());
    // durée : attribut duration du <res> — parsing d'attribut, pas de texte.
    let mut duration_ms = None;
    let mut reader = quick_xml::Reader::from_str(didl);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.rsplit(':').next().unwrap_or(&name) == "res" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"duration" {
                            let v = String::from_utf8_lossy(&attr.value).to_string();
                            duration_ms = parse_upnp_time(&v).map(|m| m as i64);
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (title, artist, duration_ms)
}

/// Parse une requête de contrôle AVTransport ou RenderingControl.
pub fn parse_renderer_command(soap_body: &str) -> RendererCommand {
    let action = crate::upnp_server::parse_soap_action(soap_body).unwrap_or_default();
    match action.as_str() {
        "SetAVTransportURI" => {
            let uri = text_of(soap_body, "CurrentURI").unwrap_or_default();
            let meta = text_of(soap_body, "CurrentURIMetaData").unwrap_or_default();
            let (title, artist, duration_ms) = parse_didl_metadata(&meta);
            RendererCommand::SetUri {
                uri,
                title,
                artist,
                duration_ms,
            }
        }
        "SetNextAVTransportURI" => {
            let uri = text_of(soap_body, "NextURI").unwrap_or_default();
            let meta = text_of(soap_body, "NextURIMetaData").unwrap_or_default();
            let (title, artist, duration_ms) = parse_didl_metadata(&meta);
            RendererCommand::SetNextUri {
                uri,
                title,
                artist,
                duration_ms,
            }
        }
        "Play" => RendererCommand::Play,
        "Pause" => RendererCommand::Pause,
        "Stop" => RendererCommand::Stop,
        "Seek" => {
            let target = text_of(soap_body, "Target").unwrap_or_default();
            match parse_upnp_time(&target) {
                Some(ms) => RendererCommand::Seek(ms),
                None => RendererCommand::Unsupported(format!("Seek({target})")),
            }
        }
        "GetTransportInfo" => RendererCommand::GetTransportInfo,
        "GetPositionInfo" => RendererCommand::GetPositionInfo,
        "GetMediaInfo" => RendererCommand::GetMediaInfo,
        "GetVolume" => RendererCommand::GetVolume,
        "SetVolume" => {
            let v = text_of(soap_body, "DesiredVolume")
                .and_then(|s| s.trim().parse::<u16>().ok())
                .map(|v| v.min(100) as u8);
            match v {
                Some(v) => RendererCommand::SetVolume(v),
                None => RendererCommand::Unsupported("SetVolume".into()),
            }
        }
        "GetMute" => RendererCommand::GetMute,
        "SetMute" => {
            let m = text_of(soap_body, "DesiredMute")
                .map(|s| matches!(s.trim(), "1" | "true" | "True" | "TRUE"));
            RendererCommand::SetMute(m.unwrap_or(false))
        }
        other => RendererCommand::Unsupported(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Réponses SOAP
// ---------------------------------------------------------------------------

fn envelope(urn: &str, action: &str, args: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action}Response xmlns:u="{urn}">{args}</u:{action}Response>
  </s:Body>
</s:Envelope>"#
    )
}

/// Réponse vide (SetAVTransportURI, Play, Pause, Stop, Seek, SetVolume, SetMute).
pub fn empty_response(action: &str) -> String {
    let urn = match action {
        "SetVolume" | "SetMute" | "GetVolume" | "GetMute" => RENDERINGCONTROL_URN,
        _ => AVTRANSPORT_URN,
    };
    envelope(urn, action, "")
}

/// État de lecture d'une zone, vu du protocole UPnP.
#[derive(Debug, Clone, Default)]
pub struct RendererSnapshot {
    /// STOPPED / PLAYING / PAUSED_PLAYBACK.
    pub transport_state: &'static str,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub uri: String,
    /// Volume 0..100.
    pub volume: u8,
    pub muted: bool,
}

pub fn transport_info_response(s: &RendererSnapshot) -> String {
    envelope(
        AVTRANSPORT_URN,
        "GetTransportInfo",
        &format!(
            "<CurrentTransportState>{}</CurrentTransportState>\
             <CurrentTransportStatus>OK</CurrentTransportStatus>\
             <CurrentSpeed>1</CurrentSpeed>",
            s.transport_state
        ),
    )
}

pub fn position_info_response(s: &RendererSnapshot) -> String {
    let dur = format_upnp_time(s.duration_ms);
    let pos = format_upnp_time(s.position_ms);
    envelope(
        AVTRANSPORT_URN,
        "GetPositionInfo",
        &format!(
            "<Track>1</Track>\
             <TrackDuration>{dur}</TrackDuration>\
             <TrackMetaData></TrackMetaData>\
             <TrackURI>{uri}</TrackURI>\
             <RelTime>{pos}</RelTime>\
             <AbsTime>{pos}</AbsTime>\
             <RelCount>2147483647</RelCount>\
             <AbsCount>2147483647</AbsCount>",
            uri = quick_xml::escape::escape(&s.uri),
        ),
    )
}

pub fn media_info_response(s: &RendererSnapshot) -> String {
    envelope(
        AVTRANSPORT_URN,
        "GetMediaInfo",
        &format!(
            "<NrTracks>1</NrTracks>\
             <MediaDuration>{}</MediaDuration>\
             <CurrentURI>{}</CurrentURI>\
             <CurrentURIMetaData></CurrentURIMetaData>",
            format_upnp_time(s.duration_ms),
            quick_xml::escape::escape(&s.uri),
        ),
    )
}

pub fn volume_response(s: &RendererSnapshot) -> String {
    envelope(
        RENDERINGCONTROL_URN,
        "GetVolume",
        &format!("<CurrentVolume>{}</CurrentVolume>", s.volume),
    )
}

pub fn mute_response(s: &RendererSnapshot) -> String {
    envelope(
        RENDERINGCONTROL_URN,
        "GetMute",
        &format!("<CurrentMute>{}</CurrentMute>", if s.muted { 1 } else { 0 }),
    )
}

// ---------------------------------------------------------------------------
// SSDP — annonces et réponses M-SEARCH des renderers actifs
// ---------------------------------------------------------------------------

/// Une annonce de renderer vivante (zone opt-in, IP connue).
#[derive(Debug, Clone)]
pub struct RendererAdvert {
    pub uuid: String,
    pub location: String,
}

/// Registre des renderers annoncés — lu par le répondeur M-SEARCH de
/// `discovery/ssdp.rs`, écrit par l'annonceur de la couche serveur. Même
/// contrat que `upnp_server::ADVERT` : mis à jour à chaque cycle d'annonce,
/// jamais figé au démarrage (#1614).
static RENDERER_ADVERTS: std::sync::RwLock<Vec<RendererAdvert>> =
    std::sync::RwLock::new(Vec::new());

pub fn set_renderer_adverts(adverts: Vec<RendererAdvert>) {
    if let Ok(mut guard) = RENDERER_ADVERTS.write() {
        *guard = adverts;
    }
}

pub fn renderer_adverts() -> Vec<RendererAdvert> {
    RENDERER_ADVERTS
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Les identités UPnP d'un MediaRenderer racine — un M-SEARCH peut viser
/// n'importe laquelle, `ssdp:all` les attend toutes.
fn renderer_usn_targets(uuid: &str) -> [(String, String); 5] {
    let device = "urn:schemas-upnp-org:device:MediaRenderer:1";
    [
        ("upnp:rootdevice".into(), format!("{uuid}::upnp:rootdevice")),
        (uuid.to_string(), uuid.to_string()),
        (device.to_string(), format!("{uuid}::{device}")),
        (
            AVTRANSPORT_URN.to_string(),
            format!("{uuid}::{AVTRANSPORT_URN}"),
        ),
        (
            RENDERINGCONTROL_URN.to_string(),
            format!("{uuid}::{RENDERINGCONTROL_URN}"),
        ),
    ]
}

/// Cibles de réponse pour un M-SEARCH `st` donné — vide si la recherche ne
/// vise pas un renderer.
pub fn renderer_msearch_targets(st: &str, uuid: &str) -> Vec<(String, String)> {
    let st = st.trim();
    if st.eq_ignore_ascii_case("ssdp:all") {
        return renderer_usn_targets(uuid).into();
    }
    renderer_usn_targets(uuid)
        .into_iter()
        .filter(|(nt, _)| nt.eq_ignore_ascii_case(st))
        .collect()
}

/// NOTIFY ssdp:alive pour chaque identité d'un renderer.
pub fn renderer_notify_messages(uuid: &str, location: &str) -> Vec<String> {
    renderer_usn_targets(uuid)
        .into_iter()
        .map(|(nt, usn)| {
            format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST: 239.255.255.250:1900\r\n\
                 CACHE-CONTROL: max-age=1800\r\n\
                 LOCATION: {location}\r\n\
                 NT: {nt}\r\n\
                 NTS: ssdp:alive\r\n\
                 SERVER: Tune/{} UPnP/1.0\r\n\
                 USN: {usn}\r\n\r\n",
                crate::version(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soap(action: &str, args: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{args}</u:{action}></s:Body>
</s:Envelope>"#
        )
    }

    #[test]
    fn parse_set_uri_avec_didl() {
        // Le DIDL arrive ÉCHAPPÉ dans CurrentURIMetaData — cas réel JPlay.
        let didl = "&lt;DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
                    xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                    xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"&gt;\
                    &lt;item&gt;&lt;dc:title&gt;So What&lt;/dc:title&gt;\
                    &lt;upnp:artist&gt;Miles Davis&lt;/upnp:artist&gt;\
                    &lt;res duration=\"0:09:22\"&gt;http://x/a.flac&lt;/res&gt;\
                    &lt;/item&gt;&lt;/DIDL-Lite&gt;";
        let body = soap(
            "SetAVTransportURI",
            &format!(
                "<InstanceID>0</InstanceID><CurrentURI>http://srv/track.flac</CurrentURI>\
                 <CurrentURIMetaData>{didl}</CurrentURIMetaData>"
            ),
        );
        match parse_renderer_command(&body) {
            RendererCommand::SetUri {
                uri,
                title,
                artist,
                duration_ms,
            } => {
                assert_eq!(uri, "http://srv/track.flac");
                assert_eq!(title.as_deref(), Some("So What"));
                assert_eq!(artist.as_deref(), Some("Miles Davis"));
                assert_eq!(duration_ms, Some(562_000));
            }
            other => panic!("attendu SetUri, obtenu {other:?}"),
        }
    }

    #[test]
    fn parse_set_next_uri() {
        let body = soap(
            "SetNextAVTransportURI",
            "<InstanceID>0</InstanceID><NextURI>http://srv/next.flac</NextURI><NextURIMetaData></NextURIMetaData>",
        );
        match parse_renderer_command(&body) {
            RendererCommand::SetNextUri { uri, .. } => assert_eq!(uri, "http://srv/next.flac"),
            other => panic!("attendu SetNextUri, obtenu {other:?}"),
        }
        assert!(avtransport_scpd().contains("<name>SetNextAVTransportURI</name>"));
    }

    #[test]
    fn parse_seek_rel_time_et_volume() {
        let body = soap(
            "Seek",
            "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>0:04:32</Target>",
        );
        assert_eq!(
            parse_renderer_command(&body),
            RendererCommand::Seek(272_000)
        );

        let vol = format!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<u:SetVolume xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>37</DesiredVolume>
</u:SetVolume></s:Body></s:Envelope>"#
        );
        assert_eq!(parse_renderer_command(&vol), RendererCommand::SetVolume(37));
    }

    #[test]
    fn temps_upnp_aller_retour() {
        assert_eq!(parse_upnp_time("0:04:32"), Some(272_000));
        assert_eq!(parse_upnp_time("1:02:03.5"), Some(3_723_500));
        assert_eq!(parse_upnp_time("04:32"), Some(272_000));
        assert_eq!(format_upnp_time(272_000), "0:04:32");
        assert_eq!(format_upnp_time(3_723_000), "1:02:03");
        assert_eq!(format_upnp_time(-5), "0:00:00");
    }

    #[test]
    fn description_et_scpd_coherents() {
        let xml = renderer_description_xml("Salon", "uuid:z1", "http://10.0.0.2:8888", 3);
        assert!(xml.contains("MediaRenderer:1"));
        assert!(xml.contains("/upnp/renderer/3/AVTransport/control"));
        assert!(xml.contains("<dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>"));
        assert!(avtransport_scpd().contains("<name>SetAVTransportURI</name>"));
        assert!(renderingcontrol_scpd().contains("<name>SetVolume</name>"));
    }

    #[test]
    fn cibles_msearch_renderer() {
        assert_eq!(renderer_msearch_targets("ssdp:all", "uuid:r").len(), 5);
        assert_eq!(
            renderer_msearch_targets("urn:schemas-upnp-org:device:MediaRenderer:1", "uuid:r").len(),
            1
        );
        assert!(
            renderer_msearch_targets("urn:schemas-upnp-org:device:MediaServer:1", "uuid:r")
                .is_empty()
        );
    }
}
