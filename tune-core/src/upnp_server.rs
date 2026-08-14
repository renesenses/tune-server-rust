//! UPnP MediaServer (ContentDirectory) implementation — business logic.
//!
//! Contains SOAP parsing, DIDL-Lite generation, SSDP advertisement helpers,
//! and the shared `UpnpState`. The Axum HTTP handlers live in
//! `tune-server/src/routes/upnp_media_server.rs`.

use quick_xml::escape::unescape;
use quick_xml::events::Event;
use tracing::{debug, warn};

use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::Track;
use crate::db::radio_repo::RadioRepo;
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::engine::Engine;
use crate::db::track_repo::TrackRepo;
use crate::discovery::ssdp;

// ---------------------------------------------------------------------------
// Shared state for UPnP routes
// ---------------------------------------------------------------------------

/// Préfixe sous lequel `tune-server` monte les routes du MediaServer
/// (`app.nest("/upnp", …)` dans `routes/mod.rs`). Le description.xml, les
/// URLs de contrôle qu'il publie et l'annonce SSDP doivent TOUS le porter :
/// en 0.9.71 le description annonçait `…/ContentDirectory/control` sans ce
/// préfixe — chaque Browse tombait sur le fallback SPA (405) et tous les
/// clients voyaient un serveur vide (#1613).
pub const MOUNT_PATH: &str = "/upnp";

/// Préfixe de l'API HTTP publique (`app.nest("/api/v1", api)` dans
/// `routes/mod.rs`). Les URL de ressource publiées dans le DIDL doivent le
/// porter, exactement comme `controlURL` doit porter [`MOUNT_PATH`] : en
/// 0.9.74 le DIDL annonçait `…/artwork/{hash}` et `…/stream/{id}.flac`, deux
/// chemins qui ne sont montés nulle part. Le premier tombait dans le fallback
/// SPA — HTTP 200 avec `Content-Type: text/html`, donc une pochette cassée
/// dans chaque client au lieu d'une erreur visible — et le second en 404
/// (`/stream/{stream_id}` attend un identifiant de session de rendu, pas un
/// `track_id`), ce qui rendait le serveur média entièrement injouable (#1681).
pub const API_PATH: &str = "/api/v1";

/// Un `cover_path` déjà stocké sous forme de condensat (le nom du fichier
/// dans le cache de pochettes), par opposition à un chemin de fichier.
fn is_hex_hash(s: &str) -> bool {
    (s.len() == 32 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// URL absolue de la pochette correspondant à un `cover_path` de la base.
///
/// Reprend à l'identique la normalisation de
/// `tune-server/src/routes/library/artwork.rs::album_artwork`, qui est la
/// seule référence : une URL distante est publiée telle quelle, un condensat
/// est servi directement, et toute autre valeur est un chemin de fichier dont
/// l'entrée de cache s'adresse par [`crate::library::artwork::artwork_hash`].
/// Concaténer `cover_path` brut derrière `/artwork/` ne fonctionnait pour
/// aucune des trois formes.
pub fn artwork_url(base_url: &str, cover_path: &str) -> String {
    if cover_path.starts_with("http://") || cover_path.starts_with("https://") {
        return cover_path.to_string();
    }
    let hash = if is_hex_hash(cover_path) {
        cover_path.to_string()
    } else {
        crate::library::artwork::artwork_hash(cover_path)
    };
    format!("{base_url}{API_PATH}/library/artwork/{hash}")
}

/// URL absolue du flux audio d'une piste, telle que servie par
/// `GET /api/v1/library/tracks/{id}/audio`.
pub fn track_audio_url(base_url: &str, track_id: i64) -> String {
    format!("{base_url}{API_PATH}/library/tracks/{track_id}/audio")
}

#[derive(Clone)]
pub struct UpnpState {
    pub backend: Arc<dyn DbBackend>,
    pub server_port: u16,
    pub friendly_name: String,
    pub uuid: String,
    /// IP forcée par la config (`advertised_ip`), prioritaire sur la
    /// détection automatique — même contrat que la chaîne de lecture
    /// (`routes/playback.rs`).
    pub advertised_ip: Option<String>,
}

impl UpnpState {
    pub fn new(
        backend: Arc<dyn DbBackend>,
        server_port: u16,
        advertised_ip: Option<String>,
    ) -> Self {
        Self {
            backend,
            server_port,
            friendly_name: "Tune Server".into(),
            uuid: format!("uuid:{}", uuid::Uuid::new_v4()),
            advertised_ip,
        }
    }

    pub fn server_ip(&self) -> String {
        if let Some(ref ip) = self.advertised_ip {
            if !ip.is_empty() {
                return ip.clone();
            }
        }
        ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into())
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.server_ip(), self.server_port)
    }
}

// ---------------------------------------------------------------------------
// Device Description XML builder
// ---------------------------------------------------------------------------

pub fn build_device_description(state: &UpnpState) -> String {
    let base = state.base_url();
    format!(
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
        <controlURL>{base}{mount}/ContentDirectory/control</controlURL>
        <eventSubURL>{base}{mount}/ContentDirectory/event</eventSubURL>
        <SCPDURL>{base}{mount}/ContentDirectory/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <controlURL>{base}{mount}/ConnectionManager/control</controlURL>
        <eventSubURL>{base}{mount}/ConnectionManager/event</eventSubURL>
        <SCPDURL>{base}{mount}/ConnectionManager/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        friendly = state.friendly_name,
        version = crate::version(),
        uuid = state.uuid,
        base = base,
        mount = MOUNT_PATH,
    )
}

// ---------------------------------------------------------------------------
// SCPD (service descriptions)
// ---------------------------------------------------------------------------

/// SCPD minimal du ContentDirectory. Le description.xml publie une SCPDURL
/// depuis toujours, mais aucune route ne la servait : les clients recevaient
/// le fallback SPA (du HTML en 200), et les points de contrôle stricts qui
/// parsent le SCPD avant de naviguer refusaient le serveur (#1613).
pub fn content_directory_scpd() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>Browse</name>
      <argumentList>
        <argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
        <argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
        <argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
        <argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
        <argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
        <argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
        <argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetSearchCapabilities</name>
      <argumentList>
        <argument><name>SearchCaps</name><direction>out</direction><relatedStateVariable>SearchCapabilities</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetSortCapabilities</name>
      <argumentList>
        <argument><name>SortCaps</name><direction>out</direction><relatedStateVariable>SortCapabilities</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetSystemUpdateID</name>
      <argumentList>
        <argument><name>Id</name><direction>out</direction><relatedStateVariable>SystemUpdateID</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType>
      <allowedValueList><allowedValue>BrowseMetadata</allowedValue><allowedValue>BrowseDirectChildren</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SortCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

/// SCPD minimal du ConnectionManager (GetProtocolInfo, la seule action que
/// le handler implémente).
pub fn connection_manager_scpd() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>GetProtocolInfo</name>
      <argumentList>
        <argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
        <argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

// ---------------------------------------------------------------------------
// ContentDirectory SOAP response builder
// ---------------------------------------------------------------------------

pub fn build_browse_response(state: &UpnpState, soap_body: &str) -> String {
    debug!(body_len = soap_body.len(), "upnp_content_directory_request");

    let (object_id, browse_flag, start, count) = parse_browse_request(soap_body);

    let direct_children = browse_flag != "BrowseMetadata";

    let didl = if direct_children {
        browse_direct_children(state, &object_id, start, count)
    } else {
        browse_metadata(state, &object_id)
    };

    let total_matches = didl.total;
    let number_returned = didl.returned;

    format!(
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
    )
}

// ---------------------------------------------------------------------------
// ConnectionManager SOAP response builder
// ---------------------------------------------------------------------------

pub fn build_connection_manager_response(soap_body: &str) -> String {
    debug!(
        body_len = soap_body.len(),
        "upnp_connection_manager_request"
    );

    r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
      <Source>http-get:*:audio/flac:*,http-get:*:audio/wav:*,http-get:*:audio/mpeg:*,http-get:*:audio/ogg:*,http-get:*:audio/aac:*,http-get:*:audio/mp4:*,http-get:*:audio/x-aiff:*</Source>
      <Sink></Sink>
    </u:GetProtocolInfoResponse>
  </s:Body>
</s:Envelope>"#
        .to_string()
}

// ---------------------------------------------------------------------------
// SOAP request parser
// ---------------------------------------------------------------------------

/// Cap applied when a control point requests "all" children (RequestedCount=0).
/// Large enough for any realistic library, small enough to stay a valid SQL
/// LIMIT on both SQLite and Postgres (unlike u64::MAX, which casts to -1).
const UNLIMITED_BROWSE_COUNT: u64 = 100_000_000;

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
                        // Per the UPnP ContentDirectory spec, RequestedCount=0
                        // means "return every child" — not "return zero". The old
                        // `if n > 0` left count at the default 100, so a control
                        // point asking for the whole library only ever saw ~100
                        // albums (Pierre M: media-server list "très incomplète,
                        // ~100 sur x xxx"). Map 0 to a large, DB-portable cap
                        // (u64::MAX -> LIMIT -1 would error on Postgres).
                        let n: u64 = text.parse().unwrap_or(0);
                        count = if n == 0 { UNLIMITED_BROWSE_COUNT } else { n };
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
    let repo = ArtistRepo::with_backend(state.backend.clone());
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
    let repo = AlbumRepo::with_backend(state.backend.clone());
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
            let url = artwork_url(&state.base_url(), cover);
            extra.push_str(&format!(
                "<upnp:albumArtURI>{}</upnp:albumArtURI>",
                quick_xml::escape::escape(&url)
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
    // Fetch distinct genres from the albums table.
    //
    // `COLLATE NOCASE` is SQLite-only — PostgreSQL rejects it — so the sort is
    // dialect-specific. `LOWER(genre)` is the portable equivalent and gives
    // the same ordering on both.
    let order = match state.backend.engine() {
        Engine::Postgres => "LOWER(genre)",
        Engine::Sqlite => "genre COLLATE NOCASE",
    };
    let genres: Vec<String> = state
        .backend
        .query_many(
            &format!(
                "SELECT DISTINCT genre FROM albums \
                 WHERE genre IS NOT NULL AND genre != '' ORDER BY {order}"
            ),
            &[],
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.as_string()))
        .collect();

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
    let repo = RadioRepo::with_backend(state.backend.clone());
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
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let albums = repo.list_by_artist(artist_id).unwrap_or_default();

    let parent_id = format!("artist/{artist_id}");
    let mut inner = String::new();
    for album in &albums {
        let id = format!("album/{}", album.id.unwrap_or(0));
        let child_count = album.track_count.map(|c| c as u64);
        let mut extra = String::new();
        if let Some(ref cover) = album.cover_path {
            extra.push_str(&format!(
                "<upnp:albumArtURI>{}</upnp:albumArtURI>",
                quick_xml::escape::escape(&artwork_url(base_url, cover))
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
    let repo = TrackRepo::with_backend(state.backend.clone());
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

    // Le `<res>` annonçait un MIME et une extension *transcodés*
    // (`…/stream/{id}.flac` pour un DSD, par exemple). Cette promesse n'a
    // jamais été tenue : aucune route ne sert `/stream/{track_id}.{ext}` et le
    // seul point d'entrée du serveur média, `/api/v1/library/tracks/{id}/audio`
    // (`routes/library/tracks.rs::stream_track_audio`), renvoie le **fichier
    // d'origine** tel quel, sans transcodage. On annonce donc le format réel du
    // fichier, calculé comme le fait ce point d'entrée pour son `Content-Type`,
    // afin que le DIDL et la réponse HTTP ne se contredisent plus (#1681).
    use crate::audio::formats::AudioFormat;
    let source_format = AudioFormat::from_extension(fmt);
    let mime = source_format
        .map(|f| f.mime_type())
        .unwrap_or("application/octet-stream");

    let stream_url = track_audio_url(base_url, track_id);
    let cover_url = track.cover_path.as_ref().map(|c| artwork_url(base_url, c));

    // Le flux étant celui du fichier source, les caractéristiques annoncées
    // sont celles du fichier source — y compris pour le DSD, où l'ancien code
    // publiait la cadence PCM (176,4/352,8 kHz) et 24 bits d'un transcodage
    // qui n'avait jamais lieu.
    let (advertised_sr, advertised_bd) = (
        track.sample_rate.map(|sr| sr as u32),
        track.bit_depth.map(|bd| bd as u32),
    );

    let mut builder = crate::outputs::didl::DidlBuilder::new(&track.title, &stream_url, mime)
        .item_id(&id)
        .parent_id(parent_id)
        .include_upnp_artist(true)
        .channels(track.channels as u32)
        .artist_opt(track.artist_name.as_deref())
        .album_opt(track.album_title.as_deref())
        .album_art_opt(cover_url.as_deref())
        .sample_rate_opt(advertised_sr)
        .bit_depth_opt(advertised_bd)
        .file_size_opt(track.file_size.map(|s| s as u64));

    if track.duration_ms > 0 {
        builder = builder.duration_ms(track.duration_ms as u64);
    }
    if track.track_number > 0 {
        builder = builder.track_number(track.track_number as u32);
    }

    builder.build_item()
}

// ---------------------------------------------------------------------------
// SSDP advertisement helper
// ---------------------------------------------------------------------------

/// Ce que le MediaServer annonce, partagé avec le listener SSDP de la
/// découverte pour qu'il puisse répondre aux M-SEARCH qui nous visent.
///
/// Sans cette réponse, Tune était invisible des points de contrôle : le
/// « Rechercher des appareils » d'un contrôleur (JPlay iOS, BubbleUPnP…)
/// envoie un M-SEARCH et écoute quelques secondes — or Tune n'émettait que
/// des NOTIFY spontanés toutes les dix minutes. Sauf coïncidence entre la
/// fenêtre d'écoute et notre annonce, le serveur n'apparaissait jamais
/// (Stéphane Villerio, 12/08/2026 : « JPlay iOS ne voit toujours pas
/// Tune », pendant que femtoServer et DMP-A6 — qui répondent au M-SEARCH —
/// figurent dans sa liste).
#[derive(Clone)]
pub struct MediaServerAdvert {
    pub uuid: String,
    pub location: String,
}

/// `RwLock`, pas `OnceLock` : l'IP annoncée doit pouvoir être rafraîchie.
/// En 0.9.71 la LOCATION était calculée UNE fois au démarrage avec un repli
/// « 127.0.0.1 » — un serveur lancé avant que le réseau soit prêt annonçait
/// du loopback à vie, NOTIFY et réponses M-SEARCH comprises (#1614).
static ADVERT: std::sync::RwLock<Option<MediaServerAdvert>> = std::sync::RwLock::new(None);

/// L'annonce du MediaServer, une fois l'annonceur démarré. `None` tant que le
/// serveur UPnP n'est pas en service (ou qu'aucune IP réseau n'est connue) —
/// le listener ne répond alors à rien.
pub fn media_server_advert() -> Option<MediaServerAdvert> {
    ADVERT.read().ok().and_then(|g| g.clone())
}

/// URL du description.xml pour une IP donnée — partage `MOUNT_PATH` avec les
/// routes HTTP pour que l'annonce et le montage ne divergent plus.
pub fn advert_location(ip: &str, port: u16) -> String {
    format!("http://{ip}:{port}{MOUNT_PATH}/description.xml")
}

/// L'IP à annoncer : `advertised_ip` de la config si renseignée, sinon la
/// détection automatique. `None` (et non « 127.0.0.1 ») quand rien n'est
/// joignable — annoncer du loopback est pire que ne rien annoncer.
fn current_advert_ip(advertised_ip: Option<&str>) -> Option<String> {
    if let Some(ip) = advertised_ip {
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }
    ssdp::get_local_ip().map(|ip| ip.to_string())
}

/// Les trois identités UPnP d'un MediaServer racine. Un M-SEARCH peut viser
/// n'importe laquelle ; `ssdp:all` attend une réponse pour chacune.
fn usn_targets(uuid: &str) -> [(String, String); 3] {
    let device = "urn:schemas-upnp-org:device:MediaServer:1";
    let service = "urn:schemas-upnp-org:service:ContentDirectory:1";
    [
        ("upnp:rootdevice".into(), format!("{uuid}::upnp:rootdevice")),
        (device.into(), format!("{uuid}::{device}")),
        (service.into(), format!("{uuid}::{service}")),
    ]
}

/// Quelles identités répondre à un M-SEARCH dont le ST est `st` — vide si la
/// recherche ne nous concerne pas (un contrôleur qui cherche des renderers,
/// par exemple, n'a pas à recevoir notre réponse).
pub fn msearch_reply_targets(st: &str, uuid: &str) -> Vec<(String, String)> {
    let st = st.trim();
    if st.eq_ignore_ascii_case("ssdp:all") {
        return usn_targets(uuid).into();
    }
    if st == uuid {
        return vec![(uuid.to_string(), uuid.to_string())];
    }
    usn_targets(uuid)
        .into_iter()
        .filter(|(nt, _)| nt.eq_ignore_ascii_case(st))
        .collect()
}

/// Réponse unicast à un M-SEARCH — le pendant « sur demande » du NOTIFY.
pub fn ssdp_msearch_response(st: &str, usn: &str, location: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         EXT:\r\n\
         LOCATION: {location}\r\n\
         SERVER: Tune/{version} UPnP/1.0\r\n\
         ST: {st}\r\n\
         USN: {usn}\r\n\
         \r\n",
        version = crate::version(),
    )
}

/// Build the SSDP NOTIFY alive payload for one (NT, USN) identity.
pub fn ssdp_notify_alive_for(nt: &str, usn: &str, location: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         LOCATION: {location}\r\n\
         NT: {nt}\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: Tune/{version} UPnP/1.0\r\n\
         USN: {usn}\r\n\
         \r\n",
        version = crate::version(),
    )
}

/// Build the SSDP NOTIFY alive payload for the MediaServer.
pub fn ssdp_notify_alive(uuid: &str, location: &str) -> String {
    ssdp_notify_alive_for(
        "urn:schemas-upnp-org:device:MediaServer:1",
        &format!("{uuid}::urn:schemas-upnp-org:device:MediaServer:1"),
        location,
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
///
/// La LOCATION est recalculée à CHAQUE cycle (l'IP peut changer : DHCP,
/// VPN, bascule d'interface), et tant qu'aucune IP réseau n'est détectée on
/// n'annonce RIEN — retry court en attendant que le réseau monte, plutôt
/// que d'annoncer un 127.0.0.1 injoignable des autres machines (#1614).
pub async fn spawn_ssdp_advertiser(uuid: String, port: u16, advertised_ip: Option<String>) {
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
        let mut network_was_up = true;

        loop {
            let Some(ip) = current_advert_ip(advertised_ip.as_deref()) else {
                // Pas d'IP réseau (démarrage avant le DHCP, interfaces
                // toutes virtuelles…) : on garde la dernière annonce connue
                // pour le répondeur M-SEARCH s'il y en avait une, on
                // n'émet pas de NOTIFY, et on réessaie vite.
                if network_was_up {
                    warn!("ssdp_advertiser_no_network_ip_waiting");
                    network_was_up = false;
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            };
            network_was_up = true;

            let location = advert_location(&ip, port);

            // Publie l'annonce pour le répondeur M-SEARCH du listener de
            // découverte : c'est lui qui rend Tune visible d'un « Rechercher
            // des appareils », le NOTIFY spontané ne couvrant que l'écoute
            // passive.
            if let Ok(mut guard) = ADVERT.write() {
                *guard = Some(MediaServerAdvert {
                    uuid: uuid.clone(),
                    location: location.clone(),
                });
            }

            // Les TROIS identités du device (rootdevice, MediaServer,
            // ContentDirectory) : certains contrôleurs ne retiennent un
            // serveur que s'ils ont vu l'identité précise qu'ils cherchent.
            for (nt, usn) in usn_targets(&uuid) {
                let p = ssdp_notify_alive_for(&nt, &usn, &location);
                if let Err(e) = socket.send_to(p.as_bytes(), dest).await {
                    debug!(error = %e, "ssdp_advertise_send_error");
                }
            }

            // 300 s, pas 600 : la spec demande de réannoncer bien avant
            // l'expiration du CACHE-CONTROL (max-age=1800) — et dix minutes
            // entre deux annonces laissaient les contrôleurs à l'écoute
            // passive nous croire disparus.
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
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
    fn parse_browse_requested_count_zero_means_all() {
        // RequestedCount=0 must map to "return every child", not the default 100.
        let soap = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>albums</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
      <StartingIndex>0</StartingIndex>
      <RequestedCount>0</RequestedCount>
    </u:Browse>
  </s:Body>
</s:Envelope>"#;

        let (_, _, _, count) = parse_browse_request(soap);
        assert_eq!(count, UNLIMITED_BROWSE_COUNT);
    }

    #[test]
    fn didl_container_escape() {
        let xml = didl_container("id", "0", "Rock & Roll", "object.container", Some(42));
        assert!(xml.contains("Rock &amp; Roll"));
        assert!(xml.contains("childCount=\"42\""));
    }

    fn test_state() -> UpnpState {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        UpnpState::new(Arc::new(db), 8888, None)
    }

    #[test]
    fn description_publie_les_urls_sous_le_prefixe_upnp() {
        // Régression #1613 : les URLs de contrôle publiées doivent porter le
        // préfixe de montage, sinon chaque Browse tombe sur le fallback SPA.
        let state = test_state();
        let xml = build_device_description(&state);
        assert!(xml.contains(&format!(
            "{MOUNT_PATH}/ContentDirectory/control</controlURL>"
        )));
        assert!(xml.contains(&format!("{MOUNT_PATH}/ContentDirectory/scpd.xml</SCPDURL>")));
        assert!(xml.contains(&format!(
            "{MOUNT_PATH}/ConnectionManager/control</controlURL>"
        )));
        assert!(!xml.contains(":8888/ContentDirectory/control"));
    }

    #[test]
    fn advertised_ip_prioritaire_sur_la_detection() {
        let mut state = test_state();
        state.advertised_ip = Some("10.11.12.13".into());
        assert_eq!(state.server_ip(), "10.11.12.13");
        assert_eq!(state.base_url(), "http://10.11.12.13:8888");
    }

    #[test]
    fn advert_location_porte_le_prefixe_de_montage() {
        assert_eq!(
            advert_location("192.168.1.41", 8888),
            "http://192.168.1.41:8888/upnp/description.xml"
        );
    }

    #[test]
    fn current_advert_ip_honore_la_config() {
        assert_eq!(
            current_advert_ip(Some("192.168.1.99")),
            Some("192.168.1.99".into())
        );
    }

    #[test]
    fn scpd_content_directory_expose_browse() {
        let scpd = content_directory_scpd();
        assert!(scpd.starts_with("<?xml"));
        assert!(scpd.contains("<name>Browse</name>"));
        assert!(scpd.contains("BrowseDirectChildren"));
        assert!(scpd.contains("urn:schemas-upnp-org:service-1-0"));
    }

    #[test]
    fn scpd_connection_manager_expose_get_protocol_info() {
        let scpd = connection_manager_scpd();
        assert!(scpd.contains("<name>GetProtocolInfo</name>"));
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

    fn sample_track() -> Track {
        Track {
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
            cover_path: Some("ce0a963bb7eb63c3b33b4e00b6ab3427".into()),
            comments: None,
        }
    }

    #[test]
    fn didl_track_formatting() {
        let track = sample_track();
        let xml = didl_track_item(&track, "album/10", "http://192.168.1.18:8085");
        assert!(xml.contains("So What"));
        assert!(xml.contains("Miles Davis"));
        assert!(xml.contains("Kind of Blue"));
        assert!(xml.contains("audio/flac"));
        assert!(xml.contains("sampleFrequency=\"96000\""));
        assert!(xml.contains("bitsPerSample=\"24\""));
        assert!(xml.contains("originalTrackNumber"));

        // Les deux URL publiées doivent viser des routes réellement montées :
        // `/stream/{id}.{ext}` répondait 404 et `/artwork/{hash}` tombait dans
        // le fallback SPA (#1681).
        assert!(
            xml.contains("http://192.168.1.18:8085/api/v1/library/tracks/42/audio"),
            "le <res> doit viser /api/v1/library/tracks/{{id}}/audio : {xml}"
        );
        assert!(!xml.contains("/stream/42."), "ancienne URL de flux : {xml}");
        assert!(
            xml.contains(
                "<upnp:albumArtURI>http://192.168.1.18:8085/api/v1/library/artwork/ce0a963bb7eb63c3b33b4e00b6ab3427</upnp:albumArtURI>"
            ),
            "pochette hors de /api/v1/library/artwork : {xml}"
        );
    }

    #[test]
    fn artwork_url_prefixe_lapi_pour_un_condensat() {
        // Forme observée en base sur .42 : les 387 albums pourvus d'une
        // pochette ont un cover_path en condensat md5.
        assert_eq!(
            artwork_url(
                "http://192.168.1.42:8888",
                "ce0a963bb7eb63c3b33b4e00b6ab3427"
            ),
            "http://192.168.1.42:8888/api/v1/library/artwork/ce0a963bb7eb63c3b33b4e00b6ab3427"
        );
    }

    #[test]
    fn artwork_url_condense_un_chemin_de_fichier() {
        // Un chemin n'est pas le nom de l'entrée de cache : il faut le
        // condenser, comme le fait `album_artwork` côté API.
        let path = "/music/Air/Talkie Walkie/cover.jpg";
        let expected = crate::library::artwork::artwork_hash(path);
        let url = artwork_url("http://host:8888", path);
        assert_eq!(
            url,
            format!("http://host:8888/api/v1/library/artwork/{expected}")
        );
        assert!(!url.contains("cover.jpg"));
    }

    #[test]
    fn artwork_url_laisse_passer_une_url_distante() {
        let remote = "https://coverart.example/release/1.jpg";
        assert_eq!(artwork_url("http://host:8888", remote), remote);
    }

    #[test]
    fn track_audio_url_vise_la_route_montee() {
        assert_eq!(
            track_audio_url("http://192.168.1.42:8888", 5239),
            "http://192.168.1.42:8888/api/v1/library/tracks/5239/audio"
        );
    }

    #[test]
    fn didl_track_annonce_le_format_source_sans_transcodage() {
        // `/api/v1/library/tracks/{id}/audio` renvoie le fichier d'origine :
        // annoncer un MIME transcodé contredisait son Content-Type.
        let mut track = sample_track();
        track.format = Some("dsf".into());
        track.sample_rate = Some(2_822_400);
        track.bit_depth = Some(1);
        let xml = didl_track_item(&track, "album/10", "http://host:8888");
        assert!(xml.contains("application/x-dsd"), "{xml}");
        assert!(!xml.contains("audio/flac"), "{xml}");
        assert!(xml.contains("sampleFrequency=\"2822400\""), "{xml}");
    }
}

#[cfg(test)]
mod ssdp_msearch_tests {
    use super::*;

    const UUID: &str = "uuid:1234";

    #[test]
    fn ssdp_all_recoit_les_trois_identites() {
        let t = msearch_reply_targets("ssdp:all", UUID);
        assert_eq!(t.len(), 3);
        assert!(t.iter().any(|(nt, _)| nt == "upnp:rootdevice"));
    }

    #[test]
    fn recherche_mediaserver_recoit_une_reponse_ciblee() {
        let t = msearch_reply_targets("urn:schemas-upnp-org:device:MediaServer:1", UUID);
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].1,
            "uuid:1234::urn:schemas-upnp-org:device:MediaServer:1"
        );
    }

    #[test]
    fn recherche_de_renderer_reste_sans_reponse() {
        // Un contrôleur qui cherche des RENDERERS ne doit pas nous voir :
        // répondre à tout polluerait la liste des autres applications.
        let t = msearch_reply_targets("urn:schemas-upnp-org:device:MediaRenderer:1", UUID);
        assert!(t.is_empty());
    }

    #[test]
    fn la_reponse_msearch_porte_st_usn_et_location() {
        let r = ssdp_msearch_response(
            "upnp:rootdevice",
            "uuid:1234::upnp:rootdevice",
            "http://x/d.xml",
        );
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains("ST: upnp:rootdevice\r\n"));
        assert!(r.contains("USN: uuid:1234::upnp:rootdevice\r\n"));
        assert!(r.contains("LOCATION: http://x/d.xml\r\n"));
        assert!(r.contains("EXT:\r\n"));
    }
}
