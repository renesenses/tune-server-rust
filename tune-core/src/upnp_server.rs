//! UPnP MediaServer (ContentDirectory) implementation.
//!
//! Advertises the Tune server as a DLNA MediaServer so DLNA clients (TVs, NAS
//! apps, network players) can browse and play the music library.
//!
//! Routes are intended to be merged into the main Axum app or served on a
//! separate port (default 8080 for UPnP description + ContentDirectory).

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use tracing::{debug, warn};

use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::Track;
use crate::db::radio_repo::RadioRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::discovery::ssdp;

// ---------------------------------------------------------------------------
// Shared state for UPnP routes
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct UpnpState {
    pub db: SqliteDb,
    pub server_port: u16,
    pub friendly_name: String,
    pub uuid: String,
}

impl UpnpState {
    pub fn new(db: SqliteDb, server_port: u16) -> Self {
        Self {
            db,
            server_port,
            friendly_name: "Tune Server".into(),
            uuid: format!("uuid:{}", uuid::Uuid::new_v4()),
        }
    }

    fn server_ip(&self) -> String {
        ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into())
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.server_ip(), self.server_port)
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<UpnpState> {
    Router::new()
        .route("/description.xml", get(device_description))
        .route("/ContentDirectory/control", post(content_directory_control))
        .route("/ContentDirectory/event", get(content_directory_event))
        .route(
            "/ConnectionManager/control",
            post(connection_manager_control),
        )
}

/// Build a standalone Axum `Router` (with state already applied) suitable for
/// merging into the main server or serving separately.
pub fn standalone_router(state: UpnpState) -> Router {
    router().with_state(state)
}

// ---------------------------------------------------------------------------
// Device Description
// ---------------------------------------------------------------------------

async fn device_description(State(state): State<UpnpState>) -> impl IntoResponse {
    let base = state.base_url();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>{friendly}</friendlyName>
    <manufacturer>MozAIk Labs</manufacturer>
    <manufacturerURL>https://mozaiklabs.fr</manufacturerURL>
    <modelDescription>Tune Music Server</modelDescription>
    <modelName>Tune</modelName>
    <modelNumber>{version}</modelNumber>
    <modelURL>https://mozaiklabs.fr/tune</modelURL>
    <UDN>{uuid}</UDN>
    <iconList>
      <icon>
        <mimetype>image/png</mimetype>
        <width>120</width><height>120</height><depth>24</depth>
        <url>/icon.png</url>
      </icon>
    </iconList>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <controlURL>{base}/ContentDirectory/control</controlURL>
        <eventSubURL>{base}/ContentDirectory/event</eventSubURL>
        <SCPDURL>{base}/ContentDirectory/scpd.xml</SCPDURL>
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
        friendly = state.friendly_name,
        version = crate::version(),
        uuid = state.uuid,
        base = base,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

// ---------------------------------------------------------------------------
// ContentDirectory control (SOAP)
// ---------------------------------------------------------------------------

async fn content_directory_control(
    State(state): State<UpnpState>,
    body: String,
) -> impl IntoResponse {
    debug!(body_len = body.len(), "upnp_content_directory_request");

    let (object_id, browse_flag, start, count) = parse_browse_request(&body);

    let direct_children = browse_flag != "BrowseMetadata";

    let didl = if direct_children {
        browse_direct_children(&state, &object_id, start, count)
    } else {
        browse_metadata(&state, &object_id)
    };

    let total_matches = didl.total;
    let number_returned = didl.returned;

    let soap = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <Result>{result}</Result>
      <NumberReturned>{returned}</NumberReturned>
      <TotalMatches>{total}</TotalMatches>
      <UpdateID>1</UpdateID>
    </u:BrowseResponse>
  </s:Body>
</s:Envelope>"#,
        result = quick_xml::escape::escape(&didl.xml),
        returned = number_returned,
        total = total_matches,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(soap))
        .unwrap()
}

// ---------------------------------------------------------------------------
// ContentDirectory event (stub — required for DLNA compliance)
// ---------------------------------------------------------------------------

async fn content_directory_event() -> impl IntoResponse {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// ConnectionManager (minimal stub)
// ---------------------------------------------------------------------------

async fn connection_manager_control(body: String) -> impl IntoResponse {
    debug!(body_len = body.len(), "upnp_connection_manager_request");

    let soap = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
      <Source>http-get:*:audio/flac:*,http-get:*:audio/wav:*,http-get:*:audio/mpeg:*,http-get:*:audio/ogg:*,http-get:*:audio/aac:*,http-get:*:audio/mp4:*,http-get:*:audio/x-aiff:*</Source>
      <Sink></Sink>
    </u:GetProtocolInfoResponse>
  </s:Body>
</s:Envelope>"#;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(soap))
        .unwrap()
}

// ---------------------------------------------------------------------------
// SOAP request parser
// ---------------------------------------------------------------------------

fn parse_browse_request(soap_xml: &str) -> (String, String, u64, u64) {
    let mut object_id = "0".to_string();
    let mut browse_flag = "BrowseDirectChildren".to_string();
    let mut start: u64 = 0;
    let mut count: u64 = 100;

    let mut reader = quick_xml::Reader::from_str(soap_xml);
    reader.config_mut().trim_text(true);
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                // Strip namespace prefix
                current_tag = name.rsplit(':').next().unwrap_or(&name).to_string();
            }
            Ok(Event::End(_)) => {
                current_tag.clear();
            }
            Ok(Event::Text(e)) => {
                let decoded = e.decode().unwrap_or_default();
                let text = match unescape(&decoded) {
                    Ok(s) => s.to_string(),
                    Err(_) => decoded.to_string(),
                };
                if text.trim().is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "ObjectID" => object_id = text,
                    "BrowseFlag" => browse_flag = text,
                    "StartingIndex" => start = text.parse().unwrap_or(0),
                    "RequestedCount" => {
                        let n: u64 = text.parse().unwrap_or(0);
                        if n > 0 {
                            count = n;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warn!(error = %e, "soap_parse_error");
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    (object_id, browse_flag, start, count)
}

// ---------------------------------------------------------------------------
// DIDL-Lite generation
// ---------------------------------------------------------------------------

struct DidlResult {
    xml: String,
    total: u64,
    returned: u64,
}

fn browse_metadata(state: &UpnpState, object_id: &str) -> DidlResult {
    // For simplicity, return the same as a single-item browse
    let result = browse_direct_children(state, object_id, 0, 1);
    DidlResult {
        xml: result.xml,
        total: 1,
        returned: 1.min(result.returned),
    }
}

fn browse_direct_children(
    state: &UpnpState,
    object_id: &str,
    start: u64,
    count: u64,
) -> DidlResult {
    let base_url = state.base_url();

    match object_id {
        "0" => browse_root(state),
        "artists" => browse_artists(state, start, count),
        "albums" => browse_albums(state, start, count),
        "genres" => browse_genres(state),
        "playlists" => browse_playlists(state),
        "radios" => browse_radios(state),
        id if id.starts_with("artist/") => {
            let artist_id: i64 = id
                .strip_prefix("artist/")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            browse_artist_albums(state, artist_id, &base_url)
        }
        id if id.starts_with("album/") => {
            let album_id: i64 = id
                .strip_prefix("album/")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            browse_album_tracks(state, album_id, &base_url)
        }
        _ => DidlResult {
            xml: didl_wrap(""),
            total: 0,
            returned: 0,
        },
    }
}

fn browse_root(_state: &UpnpState) -> DidlResult {
    let containers = [
        ("artists", "Artists", "object.container"),
        ("albums", "Albums", "object.container"),
        ("genres", "Genres", "object.container"),
        ("playlists", "Playlists", "object.container"),
        ("radios", "Radio", "object.container"),
    ];

    let mut inner = String::new();
    for (id, title, class) in &containers {
        inner.push_str(&didl_container(id, "0", title, class, None));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total: containers.len() as u64,
        returned: containers.len() as u64,
    }
}

fn browse_artists(state: &UpnpState, start: u64, count: u64) -> DidlResult {
    let repo = ArtistRepo::new(state.db.clone());
    let total = repo.count().unwrap_or(0) as u64;
    let artists = repo.list(count as i64, start as i64).unwrap_or_default();

    let mut inner = String::new();
    for artist in &artists {
        let id = format!("artist/{}", artist.id.unwrap_or(0));
        inner.push_str(&didl_container(
            &id,
            "artists",
            &artist.name,
            "object.container.person.musicArtist",
            None,
        ));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: artists.len() as u64,
    }
}

fn browse_albums(state: &UpnpState, start: u64, count: u64) -> DidlResult {
    let repo = AlbumRepo::new(state.db.clone());
    let total = repo.count().unwrap_or(0) as u64;
    let albums = repo.list(count as i64, start as i64).unwrap_or_default();

    let mut inner = String::new();
    for album in &albums {
        let id = format!("album/{}", album.id.unwrap_or(0));
        let child_count = album.track_count.map(|c| c as u64);
        let mut extra = String::new();
        if let Some(ref artist_name) = album.artist_name {
            extra.push_str(&format!(
                "<dc:creator>{}</dc:creator>",
                quick_xml::escape::escape(artist_name)
            ));
        }
        if let Some(ref cover) = album.cover_path {
            let base = state.base_url();
            extra.push_str(&format!(
                "<upnp:albumArtURI>{base}/artwork/{cover}</upnp:albumArtURI>"
            ));
        }
        inner.push_str(&didl_container_ext(
            &id,
            "albums",
            &album.title,
            "object.container.album.musicAlbum",
            child_count,
            &extra,
        ));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: albums.len() as u64,
    }
}

fn browse_genres(state: &UpnpState) -> DidlResult {
    // Fetch distinct genres from the albums table
    let conn = state.db.connection().lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT DISTINCT genre FROM albums WHERE genre IS NOT NULL AND genre != '' ORDER BY genre COLLATE NOCASE")
        .unwrap();
    let genres: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    drop(stmt);
    drop(conn);

    let mut inner = String::new();
    for genre in &genres {
        let id = format!("genre/{}", urlencoding::encode(genre));
        inner.push_str(&didl_container(
            &id,
            "genres",
            genre,
            "object.container.genre.musicGenre",
            None,
        ));
    }

    let total = genres.len() as u64;
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: total,
    }
}

fn browse_playlists(_state: &UpnpState) -> DidlResult {
    // Placeholder — playlists browsing can be extended later
    DidlResult {
        xml: didl_wrap(""),
        total: 0,
        returned: 0,
    }
}

fn browse_radios(state: &UpnpState) -> DidlResult {
    let repo = RadioRepo::new(state.db.clone());
    let stations = repo.list().unwrap_or_default();
    let _base = state.base_url();

    let mut inner = String::new();
    for station in &stations {
        let id = format!("radio/{}", station.id.unwrap_or(0));
        let mut res = String::new();
        let mime = station.codec.as_deref().unwrap_or("audio/mpeg");
        let mime_full = if mime.contains('/') {
            mime.to_string()
        } else {
            format!("audio/{mime}")
        };
        res.push_str(&format!(
            "<res protocolInfo=\"http-get:*:{mime_full}:*\">{url}</res>",
            url = quick_xml::escape::escape(&station.url),
        ));
        if let Some(ref logo) = station.logo_url {
            res.push_str(&format!(
                "<upnp:albumArtURI>{}</upnp:albumArtURI>",
                quick_xml::escape::escape(logo)
            ));
        }
        inner.push_str(&format!(
            "<item id=\"{id}\" parentID=\"radios\"><dc:title>{title}</dc:title><upnp:class>object.item.audioItem.audioBroadcast</upnp:class>{res}</item>",
            id = quick_xml::escape::escape(&id),
            title = quick_xml::escape::escape(&station.name),
            res = res,
        ));
    }

    let total = stations.len() as u64;
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: total,
    }
}

fn browse_artist_albums(state: &UpnpState, artist_id: i64, base_url: &str) -> DidlResult {
    let repo = AlbumRepo::new(state.db.clone());
    let albums = repo.list_by_artist(artist_id).unwrap_or_default();

    let parent_id = format!("artist/{artist_id}");
    let mut inner = String::new();
    for album in &albums {
        let id = format!("album/{}", album.id.unwrap_or(0));
        let child_count = album.track_count.map(|c| c as u64);
        let mut extra = String::new();
        if let Some(ref cover) = album.cover_path {
            extra.push_str(&format!(
                "<upnp:albumArtURI>{base_url}/artwork/{cover}</upnp:albumArtURI>"
            ));
        }
        inner.push_str(&didl_container_ext(
            &id,
            &parent_id,
            &album.title,
            "object.container.album.musicAlbum",
            child_count,
            &extra,
        ));
    }

    let total = albums.len() as u64;
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: total,
    }
}

fn browse_album_tracks(state: &UpnpState, album_id: i64, base_url: &str) -> DidlResult {
    let repo = TrackRepo::new(state.db.clone());
    let tracks = repo.list_by_album(album_id).unwrap_or_default();

    let parent_id = format!("album/{album_id}");
    let mut inner = String::new();
    for track in &tracks {
        inner.push_str(&didl_track_item(track, &parent_id, base_url));
    }

    let total = tracks.len() as u64;
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: total,
    }
}

// ---------------------------------------------------------------------------
// DIDL-Lite helpers
// ---------------------------------------------------------------------------

fn didl_wrap(inner: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{inner}</DIDL-Lite>"
    )
}

fn didl_container(
    id: &str,
    parent_id: &str,
    title: &str,
    class: &str,
    child_count: Option<u64>,
) -> String {
    didl_container_ext(id, parent_id, title, class, child_count, "")
}

fn didl_container_ext(
    id: &str,
    parent_id: &str,
    title: &str,
    class: &str,
    child_count: Option<u64>,
    extra_xml: &str,
) -> String {
    let cc = child_count
        .map(|c| format!(" childCount=\"{c}\""))
        .unwrap_or_default();
    format!(
        "<container id=\"{id}\" parentID=\"{pid}\"{cc}>\
         <dc:title>{title}</dc:title>\
         <upnp:class>{class}</upnp:class>\
         {extra}\
         </container>",
        id = quick_xml::escape::escape(id),
        pid = quick_xml::escape::escape(parent_id),
        title = quick_xml::escape::escape(title),
        class = class,
        extra = extra_xml,
    )
}

fn didl_track_item(track: &Track, parent_id: &str, base_url: &str) -> String {
    let track_id = track.id.unwrap_or(0);
    let id = format!("track/{track_id}");
    let fmt = track.format.as_deref().unwrap_or("flac");
    let mime = match fmt {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "aac" | "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "aiff" | "aif" => "audio/x-aiff",
        "dsf" | "dff" => "audio/x-dsd",
        "wv" => "audio/x-wavpack",
        "ape" => "audio/x-ape",
        _ => "audio/flac",
    };

    let duration_str = if track.duration_ms > 0 {
        let total_secs = track.duration_ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!(" duration=\"{h}:{m:02}:{s:02}\"")
    } else {
        String::new()
    };

    let sr_attr = track
        .sample_rate
        .map(|sr| format!(" sampleFrequency=\"{sr}\""))
        .unwrap_or_default();
    let bd_attr = track
        .bit_depth
        .map(|bd| format!(" bitsPerSample=\"{bd}\""))
        .unwrap_or_default();
    let ch_attr = format!(" nrAudioChannels=\"{}\"", track.channels);
    let size_attr = track
        .file_size
        .map(|s| format!(" size=\"{s}\""))
        .unwrap_or_default();

    let stream_url = format!("{base_url}/stream/{track_id}.{fmt}");

    let mut extra = String::new();
    if let Some(ref artist) = track.artist_name {
        extra.push_str(&format!(
            "<dc:creator>{}</dc:creator><upnp:artist>{}</upnp:artist>",
            quick_xml::escape::escape(artist),
            quick_xml::escape::escape(artist),
        ));
    }
    if let Some(ref album_title) = track.album_title {
        extra.push_str(&format!(
            "<upnp:album>{}</upnp:album>",
            quick_xml::escape::escape(album_title),
        ));
    }
    if track.track_number > 0 {
        extra.push_str(&format!(
            "<upnp:originalTrackNumber>{}</upnp:originalTrackNumber>",
            track.track_number
        ));
    }
    if let Some(ref cover) = track.cover_path {
        extra.push_str(&format!(
            "<upnp:albumArtURI>{base_url}/artwork/{cover}</upnp:albumArtURI>"
        ));
    }

    format!(
        "<item id=\"{id}\" parentID=\"{pid}\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         {extra}\
         <res protocolInfo=\"http-get:*:{mime}:*\"{duration}{sr}{bd}{ch}{size}>{url}</res>\
         </item>",
        id = quick_xml::escape::escape(&id),
        pid = quick_xml::escape::escape(parent_id),
        title = quick_xml::escape::escape(&track.title),
        extra = extra,
        mime = mime,
        duration = duration_str,
        sr = sr_attr,
        bd = bd_attr,
        ch = ch_attr,
        size = size_attr,
        url = quick_xml::escape::escape(&stream_url),
    )
}

// ---------------------------------------------------------------------------
// SSDP advertisement helper
// ---------------------------------------------------------------------------

/// Build the SSDP NOTIFY alive payload for the MediaServer.
pub fn ssdp_notify_alive(uuid: &str, location: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         LOCATION: {location}\r\n\
         NT: urn:schemas-upnp-org:device:MediaServer:1\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: Tune/{version} UPnP/1.0\r\n\
         USN: {uuid}::urn:schemas-upnp-org:device:MediaServer:1\r\n\
         \r\n",
        version = crate::version(),
    )
}

/// Build the SSDP NOTIFY bye-bye payload.
pub fn ssdp_notify_byebye(uuid: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         NT: urn:schemas-upnp-org:device:MediaServer:1\r\n\
         NTS: ssdp:byebye\r\n\
         USN: {uuid}::urn:schemas-upnp-org:device:MediaServer:1\r\n\
         \r\n"
    )
}

/// Spawn a background task that periodically sends SSDP NOTIFY alive
/// on the multicast group, advertising this server as a MediaServer.
pub async fn spawn_ssdp_advertiser(uuid: String, location: String) {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use tokio::net::UdpSocket;

    tokio::spawn(async move {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        let socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "ssdp_advertiser_bind_failed");
                return;
            }
        };

        let dest = std::net::SocketAddr::from((Ipv4Addr::new(239, 255, 255, 250), 1900u16));
        let payload = ssdp_notify_alive(&uuid, &location);

        loop {
            if let Err(e) = socket.send_to(payload.as_bytes(), dest).await {
                debug!(error = %e, "ssdp_advertise_send_error");
            }
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_browse_soap() {
        let soap = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>albums</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
      <Filter>*</Filter>
      <StartingIndex>10</StartingIndex>
      <RequestedCount>50</RequestedCount>
      <SortCriteria></SortCriteria>
    </u:Browse>
  </s:Body>
</s:Envelope>"#;

        let (object_id, browse_flag, start, count) = parse_browse_request(soap);
        assert_eq!(object_id, "albums");
        assert_eq!(browse_flag, "BrowseDirectChildren");
        assert_eq!(start, 10);
        assert_eq!(count, 50);
    }

    #[test]
    fn parse_browse_default_values() {
        let soap = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>0</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
    </u:Browse>
  </s:Body>
</s:Envelope>"#;

        let (object_id, _, start, count) = parse_browse_request(soap);
        assert_eq!(object_id, "0");
        assert_eq!(start, 0);
        assert_eq!(count, 100);
    }

    #[test]
    fn didl_container_escape() {
        let xml = didl_container("id", "0", "Rock & Roll", "object.container", Some(42));
        assert!(xml.contains("Rock &amp; Roll"));
        assert!(xml.contains("childCount=\"42\""));
    }

    #[test]
    fn ssdp_alive_message() {
        let msg = ssdp_notify_alive("uuid:1234", "http://192.168.1.18:8080/description.xml");
        assert!(msg.contains("ssdp:alive"));
        assert!(msg.contains("MediaServer:1"));
        assert!(msg.contains("uuid:1234"));
    }

    #[test]
    fn ssdp_byebye_message() {
        let msg = ssdp_notify_byebye("uuid:1234");
        assert!(msg.contains("ssdp:byebye"));
        assert!(msg.contains("uuid:1234"));
    }

    #[test]
    fn didl_track_formatting() {
        let track = Track {
            id: Some(42),
            title: "So What".into(),
            album_id: Some(10),
            album_title: Some("Kind of Blue".into()),
            artist_id: Some(1),
            artist_name: Some("Miles Davis".into()),
            album_artist: None,
            disc_number: 1,
            disc_subtitle: None,
            track_number: 1,
            duration_ms: 562_000,
            file_path: Some("/music/so_what.flac".into()),
            format: Some("flac".into()),
            sample_rate: Some(96000),
            bit_depth: Some(24),
            channels: 2,
            file_mtime: None,
            file_size: Some(50_000_000),
            audio_hash: None,
            source: "local".into(),
            source_id: None,
            isrc: None,
            genre: None,
            genres: None,
            composer: None,
            year: None,
            bpm: None,
            label: None,
            musicbrainz_recording_id: None,
            cover_path: Some("abc123".into()),
        };
        let xml = didl_track_item(&track, "album/10", "http://192.168.1.18:8085");
        assert!(xml.contains("So What"));
        assert!(xml.contains("Miles Davis"));
        assert!(xml.contains("Kind of Blue"));
        assert!(xml.contains("audio/flac"));
        assert!(xml.contains("stream/42.flac"));
        assert!(xml.contains("sampleFrequency=\"96000\""));
        assert!(xml.contains("bitsPerSample=\"24\""));
        assert!(xml.contains("albumArtURI"));
        assert!(xml.contains("originalTrackNumber"));
    }
}
