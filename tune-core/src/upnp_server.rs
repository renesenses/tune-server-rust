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
use crate::db::playlist_repo::PlaylistRepo;
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

/// URL stable du flux radio servi par Tune au MediaServer.
///
/// La station externe n'est volontairement pas publiée dans la DIDL : le
/// renderer revient vers Tune, qui peut alors décoder le codec source en WAV,
/// appliquer son contrat HTTP live et nettoyer la session à la déconnexion.
pub fn radio_audio_url(base_url: &str, radio_id: i64) -> String {
    format!("{base_url}{API_PATH}/radios/{radio_id}/audio.wav")
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
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        // UDN stable entre les démarrages : certains points de contrôle (JPLAY
        // notamment) mémorisent un MediaServer par UDN. Un uuid régénéré à
        // chaque boot faisait apparaître un « nouveau » serveur à chaque
        // redémarrage et cassait l'appairage mémorisé.
        let uuid = match settings
            .get("upnp_udn")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
        {
            Some(u) => u,
            None => {
                let fresh = format!("uuid:{}", uuid::Uuid::new_v4());
                let _ = settings.set("upnp_udn", &fresh);
                fresh
            }
        };
        // Le nom publié suit le réglage `upnp_friendly_name` (POST
        // /api/v1/upnp/config) — il était écrit mais jamais lu, renommer le
        // serveur depuis l'interface n'avait aucun effet.
        let friendly_name = settings
            .get("upnp_friendly_name")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "Tune Server".into());
        Self {
            backend,
            server_port,
            friendly_name,
            uuid,
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
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>
    <friendlyName>{friendly}</friendlyName>
    <manufacturer>MozAIk Labs</manufacturer>
    <manufacturerURL>https://mozaiklabs.fr</manufacturerURL>
    <modelDescription>Tune Music Server</modelDescription>
    <modelName>Tune</modelName>
    <modelNumber>{version}</modelNumber>
    <modelURL>https://mozaiklabs.fr/tune</modelURL>
    <serialNumber>{version}</serialNumber>
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
      <name>Search</name>
      <argumentList>
        <argument><name>ContainerID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
        <argument><name>SearchCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SearchCriteria</relatedStateVariable></argument>
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
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SearchCriteria</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SortCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

/// SCPD du ConnectionManager. Les trois actions obligatoires de CM:1 —
/// un point de contrôle strict peut appeler GetCurrentConnectionIDs/Info
/// avant son premier Browse et refuser un serveur qui ne les déclare pas.
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
    <action>
      <name>GetCurrentConnectionIDs</name>
      <argumentList>
        <argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetCurrentConnectionInfo</name>
      <argumentList>
        <argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
        <argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
        <argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>
        <argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>
        <argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>
        <argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType>
      <allowedValueList><allowedValue>Input</allowedValue><allowedValue>Output</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType>
      <allowedValueList><allowedValue>OK</allowedValue><allowedValue>ContentFormatMismatch</allowedValue><allowedValue>InsufficientBandwidth</allowedValue><allowedValue>UnreliableChannel</allowedValue><allowedValue>Unknown</allowedValue></allowedValueList>
    </stateVariable>
  </serviceStateTable>
</scpd>"#
}

// ---------------------------------------------------------------------------
// ContentDirectory SOAP response builder
// ---------------------------------------------------------------------------

/// Nom de la première action SOAP du corps (élément fils de `Body`), sans
/// préfixe de namespace. `None` si le corps n'est pas du SOAP reconnaissable.
pub fn parse_soap_action(soap_xml: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(soap_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_body = false;
    loop {
        match reader.read_event_into(&mut buf) {
            // `Empty` autant que `Start` : une action SANS argument s'ecrit
            // legitimement `<u:GetSearchCapabilities/>`, et quick-xml la rend
            // comme `Event::Empty`. Ne lire que `Start` faisait rendre `None`,
            // donc — par le repli historique — une BrowseResponse a la place
            // des capacites. Verifie en direct sur .18 en 0.9.103 : la forme
            // auto-fermante ramenait la racine du serveur, la forme ouverte
            // `<SearchCaps>upnp:class</SearchCaps>`.
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if in_body {
                    return Some(local);
                }
                if local == "Body" {
                    in_body = true;
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Object addressed by a ContentDirectory request, when the action carries
/// one. Browse calls it `ObjectID`; Search and several optional actions call
/// the same concept `ContainerID`.
fn parse_content_directory_object_id(soap_xml: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(soap_xml);
    reader.config_mut().trim_text(true);
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_tag = name.rsplit(':').next().unwrap_or(&name).to_string();
            }
            Ok(Event::End(_)) => current_tag.clear(),
            Ok(Event::Text(e)) if matches!(current_tag.as_str(), "ObjectID" | "ContainerID") => {
                let decoded = e.decode().ok()?;
                let value = match unescape(&decoded) {
                    Ok(unescaped) => unescaped.into_owned(),
                    Err(_) => decoded.to_string(),
                };
                let value = value.trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Enveloppe SOAP d'une réponse d'action réussie.
fn soap_action_response(service_urn: &str, action: &str, args: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action}Response xmlns:u="{urn}">{args}</u:{action}Response>
  </s:Body>
</s:Envelope>"#,
        action = action,
        urn = service_urn,
        args = args,
    )
}

/// Fault SOAP UPnP (à servir en HTTP 500 côté route).
pub fn soap_fault(error_code: u32, description: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <s:Fault>
      <faultcode>s:Client</faultcode>
      <faultstring>UPnPError</faultstring>
      <detail>
        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>{code}</errorCode>
          <errorDescription>{desc}</errorDescription>
        </UPnPError>
      </detail>
    </s:Fault>
  </s:Body>
</s:Envelope>"#,
        code = error_code,
        desc = quick_xml::escape::escape(description),
    )
}

/// Une réponse SOAP est-elle un fault ? Les routes s'en servent pour renvoyer
/// le statut HTTP 500 que la spec impose aux faults.
pub fn is_soap_fault(soap: &str) -> bool {
    soap.contains("<s:Fault>")
}

/// SID pour une réponse SUBSCRIBE (GENA). Aucun état n'est conservé : le
/// serveur n'émet pas d'événements, mais un SUBSCRIBE qui échoue suffit à
/// faire abandonner certains points de contrôle avant le premier Browse.
pub fn new_subscription_sid() -> String {
    format!("uuid:{}", uuid::Uuid::new_v4())
}

const CONTENT_DIRECTORY_URN: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CONNECTION_MANAGER_URN: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Point d'entrée du contrôle ContentDirectory : dispatch sur le NOM d'action.
/// Avant, toute requête recevait une BrowseResponse — un point de contrôle
/// strict qui appelle `GetSortCapabilities` avant son premier Browse (JPLAY)
/// recevait un corps dont l'élément ne correspond pas à l'action et abandonnait.
pub fn build_browse_response(state: &UpnpState, soap_body: &str) -> String {
    match parse_soap_action(soap_body).as_deref() {
        // Corps sans action identifiable : on garde le comportement historique
        // (Browse) plutôt que de casser un client laxiste qui marchait.
        None | Some("Browse") => browse_action_response(state, soap_body),
        Some("GetSortCapabilities") => soap_action_response(
            CONTENT_DIRECTORY_URN,
            "GetSortCapabilities",
            "<SortCaps></SortCaps>",
        ),
        Some("GetSearchCapabilities") => soap_action_response(
            CONTENT_DIRECTORY_URN,
            "GetSearchCapabilities",
            &format!("<SearchCaps>{SEARCH_CAPS}</SearchCaps>"),
        ),
        Some("Search") => search_action_response(state, soap_body),
        Some("GetSystemUpdateID") => {
            soap_action_response(CONTENT_DIRECTORY_URN, "GetSystemUpdateID", "<Id>1</Id>")
        }
        Some(other) => {
            let object_id = parse_content_directory_object_id(soap_body);
            warn!(
                action = other,
                object_id = object_id.as_deref().unwrap_or("<absent>"),
                "upnp_content_directory_unsupported_action"
            );
            soap_fault(401, "Invalid Action")
        }
    }
}

/// Ce sur quoi nous savons chercher, annonce a `GetSearchCapabilities`.
///
/// Un `SearchCaps` VIDE dit « je ne sais rien chercher ». C'est ce que Tune
/// annoncait — tout en repondant `401 Invalid Action` a l'action `Search`
/// elle-meme. Les points de controle qui INDEXENT une bibliotheque (JPlay,
/// entre autres) s'en servent pour recuperer toutes les pistes en une passe
/// paginee, bien plus vite qu'un parcours recursif : ils echouaient donc a
/// synchroniser, sans que rien ne dise pourquoi (Patatorz, fil forum #1516).
/// `dc:title` s'y ajoute depuis que `Search` sait reellement filtrer sur le
/// titre : la regle reste celle de #2312 — n'annoncer QUE ce qu'on evalue,
/// jamais l'inverse. Le test `les_capacites_annoncees_sont_toutes_evaluees`
/// tient l'invariant dans les deux sens.
const SEARCH_CAPS: &str = "upnp:class,dc:title";

/// L'action `Search` de ContentDirectory.
///
/// Portee volontairement etroite, et annoncee comme telle : on sait rendre
/// **ce que `browse_*` publie deja** — pistes, radios, artistes, albums,
/// genres. C'est ce que demandent les clients d'indexation et les menus des
/// lecteurs reseau, et c'est tout ce qu'on puisse servir sans inventer un
/// moteur de criteres complet : un `SearchCriteria` peut porter des
/// expressions booleennes arbitraires que personne ici ne sait evaluer.
///
/// Un critere qui ne vise aucune classe publiee rend une liste VIDE plutot
/// qu'une faute : un client qui cherche des images ou des videos doit lire
/// « rien de tel ici », pas « ce serveur est casse ».
///
/// La pagination des pistes est celle de `browse_all_tracks`, deja eprouvee —
/// le client redemande par tranches, exactement comme sur « All Tracks ».
fn search_action_response(state: &UpnpState, soap_body: &str) -> String {
    let (container_id, criteria, start, count, sort_criteria) = parse_search_request(soap_body);
    if !sort_criteria.trim().is_empty() {
        return soap_fault(709, "Unsupported or invalid sort criteria");
    }
    let criteres = match evaluer_criteres(&criteria) {
        Ok(c) => c,
        Err(()) => {
            debug!(criteria = %criteria, "upnp_search_criteria_non_supporte");
            return soap_fault(708, "Unsupported or invalid search criteria");
        }
    };
    let base_url = state.base_url();

    let didl = match criteres.cible {
        Some(CibleRecherche::Pistes) => match search_tracks_in_container(
            state,
            &container_id,
            start,
            count,
            &base_url,
            &criteres.titres,
        ) {
            Some(result) => result,
            None => return soap_fault(710, "No such container"),
        },
        Some(cible) => {
            match search_containers_in_container(
                state,
                cible,
                &container_id,
                start,
                count,
                &criteres.titres,
            ) {
                Some(result) => result,
                None => return soap_fault(710, "No such container"),
            }
        }
        None => empty_didl(),
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:SearchResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <Result>{result}</Result>
      <NumberReturned>{returned}</NumberReturned>
      <TotalMatches>{total}</TotalMatches>
      <UpdateID>1</UpdateID>
    </u:SearchResponse>
  </s:Body>
</s:Envelope>"#,
        result = quick_xml::escape::escape(&didl.xml),
        returned = didl.returned,
        total = didl.total,
    )
}

/// La rubrique qu'un `SearchCriteria` vise, une fois réduite à ce que Tune
/// publie réellement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CibleRecherche {
    Pistes,
    Radios,
    Artistes,
    Albums,
    Genres,
}

impl CibleRecherche {
    /// Le conteneur racine où cette rubrique se parcourt. C'est, avec la
    /// racine « 0 », la seule portée où une recherche de cette classe a un
    /// sens : chercher des artistes DANS un album ne rend rien.
    fn conteneur_racine(self) -> &'static str {
        match self {
            CibleRecherche::Pistes => "tracks",
            CibleRecherche::Radios => "radios",
            CibleRecherche::Artistes => "artists",
            CibleRecherche::Albums => "albums",
            CibleRecherche::Genres => "genres",
        }
    }
}

/// Les classes DIDL que Tune publie RÉELLEMENT, avec les ancêtres qu'un
/// `derivedfrom` a le droit de nommer pour les atteindre.
///
/// `Search` ne peut rendre que ce qui existe : cette table est la liste
/// exhaustive, et c'est elle qui dit quelle rubrique une expression vise.
/// Les cinq entrées correspondent une à une aux `browse_*` : `browse_all_tracks`,
/// `browse_radios`, `browse_artists`, `browse_albums`, `browse_genres`.
///
/// Les ancêtres sont volontairement PROCHES. `object.item`, `object.container`
/// ou `object` balaieraient tout, et c'est exactement le garde que tient le
/// test `une_recherche_d_images_ou_de_videos_ne_rend_rien` : sans lui,
/// « object.item.imageItem » passerait par la clause `object.item` et rendrait
/// toute la discothèque à un client qui cherche des photos.
const CLASSES_PUBLIEES: [(CibleRecherche, &str, &[&str]); 5] = [
    (
        CibleRecherche::Pistes,
        "object.item.audioitem.musictrack",
        &["object.item.audioitem"],
    ),
    (
        CibleRecherche::Radios,
        "object.item.audioitem.audiobroadcast",
        &["object.item.audioitem"],
    ),
    (
        CibleRecherche::Artistes,
        "object.container.person.musicartist",
        &["object.container.person"],
    ),
    (
        CibleRecherche::Albums,
        "object.container.album.musicalbum",
        &["object.container.album"],
    ),
    (
        CibleRecherche::Genres,
        "object.container.genre.musicgenre",
        &["object.container.genre"],
    ),
];

/// Un prédicat `upnp:class` appliqué à UNE classe publiée.
fn predicat_de_classe(op: &str, valeur: &str, classe: &str, ancetres: &[&str]) -> Result<bool, ()> {
    match op {
        "=" => Ok(classe == valeur),
        "!=" => Ok(classe != valeur),
        "contains" => Ok(classe.contains(valeur)),
        "doesnotcontain" => Ok(!classe.contains(valeur)),
        "derivedfrom" => Ok(valeur == classe || ancetres.iter().any(|a| *a == valeur)),
        _ => Err(()),
    }
}

/// Évalue le sous-ensemble de SearchCriteria réellement annoncé, et rend les
/// rubriques que le prédicat laisse passer.
///
/// Tune annonce `upnp:class` et `dc:title`. Toute expression qui mentionne un
/// autre champ reçoit le SOAP 708 prévu par ContentDirectory, au lieu de
/// rendre mensongèrement toute la bibliothèque.
///
/// `*` reste le raccourci d'indexation historique — il laisse passer tout ce
/// qu'on publie, et [`cible_unique`] le ramène aux pistes.
fn cibles_du_predicat(criteria: &str) -> Result<Vec<CibleRecherche>, ()> {
    let c = criteria.trim();
    if c == "*" {
        return Ok(CLASSES_PUBLIEES
            .iter()
            .map(|(cible, _, _)| *cible)
            .collect());
    }
    let parts: Vec<&str> = c.split_whitespace().collect();
    if parts.len() != 3 || !parts[0].eq_ignore_ascii_case("upnp:class") {
        return Err(());
    }
    let valeur = parts[2]
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or(())?
        .to_ascii_lowercase();
    let op = parts[1].to_ascii_lowercase();
    let mut retenues = Vec::new();
    for (cible, classe, ancetres) in CLASSES_PUBLIEES {
        if predicat_de_classe(&op, &valeur, classe, ancetres)? {
            retenues.push(cible);
        }
    }
    Ok(retenues)
}

/// Une expression de classe peut laisser passer PLUSIEURS rubriques :
/// `derivedfrom "object.item.audioItem"` vise à la fois les pistes et les
/// radios. La règle est choisie pour ne rien changer à ce qui marchait — les
/// pistes l'emportent, parce que c'est le parcours d'indexation historique
/// (#1516) et qu'il doit rendre exactement la même chose qu'avant.
///
/// Sinon une rubrique unique est servie. Une ambiguïté entre plusieurs
/// rubriques non-pistes rend une liste vide plutôt qu'un mélange qu'aucun
/// point de contrôle ne saurait paginer.
fn cible_unique(cibles: &[CibleRecherche]) -> Option<CibleRecherche> {
    if cibles.contains(&CibleRecherche::Pistes) {
        return Some(CibleRecherche::Pistes);
    }
    match cibles {
        [seule] => Some(*seule),
        _ => None,
    }
}

/// Le prédicat vu du seul point de vue des PISTES.
///
/// Forme historique conservée : c'est elle qui porte l'invariant de #2312 —
/// n'annoncer que ce qu'on évalue — et les tests qui le tiennent.
fn evaluate_supported_class_criteria(criteria: &str) -> Result<bool, ()> {
    Ok(cibles_du_predicat(criteria)?.contains(&CibleRecherche::Pistes))
}

/// Un predicat sur `dc:title`.
///
/// La comparaison est celle de la base : `LIKE` insensible a la casse ET aux
/// accents (`LOWER(unaccent(...))`, `search_by_title`). On la reproduit a
/// l'identique en memoire, sinon un meme critere rendrait deux resultats
/// differents selon le conteneur interroge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PredicatTitre {
    op: OpTitre,
    valeur: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OpTitre {
    Contient,
    NeContientPas,
    Egal,
    Different,
}

impl PredicatTitre {
    fn satisfait(&self, titre: &str) -> bool {
        let t = sans_accents_minuscule(titre);
        let v = sans_accents_minuscule(&self.valeur);
        match self.op {
            OpTitre::Contient => t.contains(&v),
            OpTitre::NeContientPas => !t.contains(&v),
            OpTitre::Egal => t == v,
            OpTitre::Different => t != v,
        }
    }

    /// La valeur a pousser dans le `LIKE` de la base, quand ce predicat peut
    /// servir de pre-filtre. Un predicat NEGATIF n'en est pas un : il ne
    /// reduit rien.
    fn valeur_prefiltrante(&self) -> Option<&str> {
        match self.op {
            OpTitre::Contient | OpTitre::Egal => Some(&self.valeur),
            OpTitre::NeContientPas | OpTitre::Different => None,
        }
    }
}

/// Repli le titre comme le fait `unaccent` cote base, pour les diacritiques
/// latins courants. Ce n'est pas une normalisation Unicode complete — c'est
/// ce que la base applique, et les deux doivent dire la meme chose.
fn sans_accents_minuscule(s: &str) -> String {
    s.chars()
        // `to_ascii_lowercase` laisse « É » intact : il faut la minuscule
        // Unicode AVANT de replier, sinon « Élégie » ne repond pas a
        // « elegie » — le cas exact que le test tient.
        .flat_map(|c| c.to_lowercase())
        .flat_map(|c| {
            let remplace = match c {
                'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => Some('a'),
                'ç' => Some('c'),
                'è' | 'é' | 'ê' | 'ë' => Some('e'),
                'ì' | 'í' | 'î' | 'ï' => Some('i'),
                'ñ' => Some('n'),
                'ò' | 'ó' | 'ô' | 'õ' | 'ö' => Some('o'),
                'ù' | 'ú' | 'û' | 'ü' => Some('u'),
                'ý' | 'ÿ' => Some('y'),
                _ => None,
            };
            std::iter::once(remplace.unwrap_or(c))
        })
        .collect()
}

/// Ce qu'un `SearchCriteria` demande, une fois reduit a ce qu'on sait faire.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CriteresRecherche {
    /// La rubrique visee, ou `None` si aucune classe publiee ne convient —
    /// une recherche de photos, par exemple, rend une liste vide.
    pub(crate) cible: Option<CibleRecherche>,
    pub(crate) titres: Vec<PredicatTitre>,
}

/// Analyse le `SearchCriteria` et le reduit aux champs ANNONCES.
///
/// Portee, volontairement etroite et alignee sur `SEARCH_CAPS` :
/// - `*`, le raccourci d'indexation historique ;
/// - des predicats sur `upnp:class` et `dc:title` ;
/// - leur conjonction par `and`.
///
/// Tout le reste — `or`, parentheses, autre champ, `exists` — rend `Err`, donc
/// un SOAP 708. C'est la lecon de #2312 : mieux vaut refuser explicitement que
/// rendre la bibliotheque entiere en faisant croire qu'on a cherche.
pub(crate) fn evaluer_criteres(criteria: &str) -> Result<CriteresRecherche, ()> {
    let c = criteria.trim();
    if c == "*" {
        return Ok(CriteresRecherche {
            cible: Some(CibleRecherche::Pistes),
            titres: Vec::new(),
        });
    }
    if c.contains('(') || c.contains(')') {
        return Err(());
    }

    // On part de TOUT ce qu'on publie, et chaque predicat de classe restreint.
    // La conjonction se lit donc comme une intersection, ce qu'elle est.
    let mut cibles: Vec<CibleRecherche> = CLASSES_PUBLIEES
        .iter()
        .map(|(cible, _, _)| *cible)
        .collect();
    let mut titres = Vec::new();
    for predicat in decouper_conjonction(c)? {
        let (champ, op, valeur) = decouper_predicat(&predicat)?;
        if champ.eq_ignore_ascii_case("upnp:class") {
            let retenues = cibles_du_predicat(&format!("{champ} {op} \"{valeur}\""))?;
            cibles.retain(|cible| retenues.contains(cible));
        } else if champ.eq_ignore_ascii_case("dc:title") {
            let op = match op.to_ascii_lowercase().as_str() {
                "contains" => OpTitre::Contient,
                "doesnotcontain" => OpTitre::NeContientPas,
                "=" => OpTitre::Egal,
                "!=" => OpTitre::Different,
                _ => return Err(()),
            };
            titres.push(PredicatTitre { op, valeur });
        } else {
            return Err(());
        }
    }
    Ok(CriteresRecherche {
        cible: cible_unique(&cibles),
        titres,
    })
}

/// Coupe sur les `and` de premier niveau, en respectant les guillemets — un
/// titre peut contenir « and », et le couper la ferait chercher n'importe quoi.
fn decouper_conjonction(c: &str) -> Result<Vec<String>, ()> {
    let mut parties = Vec::new();
    let mut courant = String::new();
    let mut dans_guillemets = false;
    let mut mots = Vec::new();
    for ch in c.chars() {
        if ch == '"' {
            dans_guillemets = !dans_guillemets;
            courant.push(ch);
        } else if ch.is_whitespace() && !dans_guillemets {
            if !courant.is_empty() {
                mots.push(std::mem::take(&mut courant));
            }
        } else {
            courant.push(ch);
        }
    }
    if dans_guillemets {
        return Err(());
    }
    if !courant.is_empty() {
        mots.push(courant);
    }

    let mut bloc: Vec<String> = Vec::new();
    for mot in mots {
        if mot.eq_ignore_ascii_case("or") {
            return Err(());
        }
        if mot.eq_ignore_ascii_case("and") {
            if bloc.is_empty() {
                return Err(());
            }
            parties.push(bloc.join(" "));
            bloc = Vec::new();
        } else {
            bloc.push(mot);
        }
    }
    if bloc.is_empty() {
        return Err(());
    }
    parties.push(bloc.join(" "));
    Ok(parties)
}

/// `champ op "valeur"` — la valeur garde ses espaces.
fn decouper_predicat(p: &str) -> Result<(String, String, String), ()> {
    let mut it = p.splitn(3, ' ');
    let champ = it.next().ok_or(())?.to_string();
    let op = it.next().ok_or(())?.to_string();
    let brut = it.next().ok_or(())?.trim().to_string();
    let valeur = brut
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or(())?
        .to_string();
    Ok((champ, op, valeur))
}

/// Pistes situées sous le conteneur demandé, avec pagination et total filtré.
///
/// Les conteneurs synthétiques racine mènent tous à la même bibliothèque à
/// plat. Les conteneurs album/artiste restreignent réellement les résultats ;
/// auparavant leur identifiant était lu puis ignoré et une recherche dans un
/// album pouvait ressortir toute la discothèque (#2312).
fn search_tracks_in_container(
    state: &UpnpState,
    container_id: &str,
    start: u64,
    count: u64,
    base_url: &str,
    titres: &[PredicatTitre],
) -> Option<DidlResult> {
    match container_id {
        "0" | "tracks" | "artists" | "albums" | "genres" | "years" | "playlists" => {
            if titres.is_empty() {
                // Sans predicat de titre, c'est le parcours d'indexation :
                // la pagination reste celle de la base, pas de la memoire.
                return Some(browse_all_tracks(state, start, count, base_url));
            }
            let tracks = candidats_par_titre(state, titres)?;
            Some(paginate_track_results(
                tracks, "tracks", start, count, base_url,
            ))
        }
        "radios" => Some(empty_didl()),
        id if id.starts_with("album/") => {
            let album_id = id.strip_prefix("album/")?.parse().ok()?;
            let tracks = TrackRepo::with_backend(state.backend.clone())
                .list_by_album(album_id)
                .ok()?;
            Some(paginate_track_results(
                filtrer_par_titre(tracks, titres),
                id,
                start,
                count,
                base_url,
            ))
        }
        id if id.starts_with("artist/") => {
            let artist_id = id.strip_prefix("artist/")?.parse().ok()?;
            let tracks = TrackRepo::with_backend(state.backend.clone())
                .list_by_artist(artist_id)
                .ok()?;
            Some(paginate_track_results(
                filtrer_par_titre(tracks, titres),
                id,
                start,
                count,
                base_url,
            ))
        }
        // Chercher DANS une liste de lecture restreint réellement aux pistes de
        // cette liste, comme pour un album — et dans l'ordre de la liste.
        id if id.starts_with("playlist/") => {
            let playlist_id = decode_playlist_id(id)?;
            let ids = PlaylistRepo::with_backend(state.backend.clone())
                .get_track_ids(playlist_id)
                .ok()?;
            let tracks = pistes_dans_l_ordre(state, &ids);
            Some(paginate_track_results(
                filtrer_par_titre(tracks, titres),
                id,
                start,
                count,
                base_url,
            ))
        }
        _ => None,
    }
}

/// Assez large pour une bibliotheque reelle (2 222 albums sur la
/// bibliotheque de reference), assez borne pour qu'un `Search` ne batisse
/// jamais un DIDL de plusieurs megaoctets en memoire. Meme regle et meme
/// ordre de grandeur que `candidats_par_titre` pour les pistes.
const MAX_CANDIDATS_CONTENEURS: i64 = 10_000;

/// Les rubriques NON-pistes d'un `Search` : artistes, albums, genres, radios.
///
/// C'est le trou que decrit le fil forum #1439 (#1777, Jean Valjean, Marantz
/// ND8006, releve du 30/08/2026) : le meme serveur montre ses conteneurs
/// PLEINS par « Parcourir les dossiers » — le chemin `Browse` — et VIDES par
/// les entrees Artistes / Albums / Genres / Radios du menu de l'appareil, qui
/// passent par `Search`. Seule « Titres » repondait, parce que `Search` ne
/// connaissait qu'une classe, `object.item.audioItem.musicTrack` : toute
/// expression visant un conteneur retombait sur un DIDL vide, sans faute ni
/// trace. Les rubriques existent pourtant deja — ce sont celles de `browse_*`,
/// et ce sont leurs emetteurs DIDL qui servent ici.
///
/// La lecture est BORNEE puis paginee en memoire, parce qu'un predicat de
/// titre doit s'appliquer AVANT la page — sinon deux pages successives ne
/// porteraient pas sur le meme ensemble. `TotalMatches` reflete donc ce qui a
/// ete retenu, comme pour `candidats_par_titre`.
fn search_containers_in_container(
    state: &UpnpState,
    cible: CibleRecherche,
    container_id: &str,
    start: u64,
    count: u64,
    titres: &[PredicatTitre],
) -> Option<DidlResult> {
    // Chercher des artistes DANS un album n'a pas de sens : la liste est vide,
    // elle n'est pas fautive. Seul un identifiant qu'on ne publie nulle part
    // merite le 710 — c'est la meme distinction que fait `search_tracks_in_container`
    // entre ses branches connues et son bras par defaut.
    if container_id != "0" && container_id != cible.conteneur_racine() {
        return if conteneur_publie(container_id) {
            Some(empty_didl())
        } else {
            None
        };
    }
    let base_url = state.base_url();
    match cible {
        // Traitees par `search_tracks_in_container`, qui pagine en base.
        CibleRecherche::Pistes => None,
        CibleRecherche::Artistes => {
            let artistes = ArtistRepo::with_backend(state.backend.clone())
                .list(MAX_CANDIDATS_CONTENEURS, 0)
                .unwrap_or_default();
            let retenus = retenir_par_titre(artistes, titres, |a| a.name.as_str());
            let (page, total) = paginer(retenus, start, count);
            Some(didl_artistes(state, &page, "artists", total))
        }
        CibleRecherche::Albums => {
            let albums = AlbumRepo::with_backend(state.backend.clone())
                .list(MAX_CANDIDATS_CONTENEURS, 0)
                .unwrap_or_default();
            let retenus = retenir_par_titre(albums, titres, |a| a.title.as_str());
            let (page, total) = paginer(retenus, start, count);
            let mut didl = didl_albums_under(&page, "albums", &base_url);
            didl.total = total;
            Some(didl)
        }
        CibleRecherche::Genres => {
            let genres = lire_genres(state);
            let retenus = retenir_par_titre(genres, titres, |g| g.as_str());
            let (page, total) = paginer(retenus, start, count);
            let mut didl = didl_genres(&page);
            didl.total = total;
            Some(didl)
        }
        CibleRecherche::Radios => {
            let stations = RadioRepo::with_backend(state.backend.clone())
                .list()
                .unwrap_or_default();
            let retenues = retenir_par_titre(stations, titres, |s| s.name.as_str());
            let (page, total) = paginer(retenues, start, count);
            let mut didl = didl_radios(&page, &base_url);
            didl.total = total;
            Some(didl)
        }
    }
}

/// Les identifiants de conteneur que le serveur publie — la racine, les
/// rubriques de [`ROOT_CONTAINERS`] et leurs enfants navigables. La liste doit
/// suivre `browse_direct_children` : un conteneur qu'on sait ouvrir doit se
/// laisser interroger, meme pour rendre une liste vide.
fn conteneur_publie(id: &str) -> bool {
    id == "0"
        || ROOT_CONTAINERS.iter().any(|(racine, _, _)| *racine == id)
        || ["artist/", "album/", "genre/", "year/", "playlist/"]
            .iter()
            .any(|prefixe| id.starts_with(prefixe))
}

/// Applique les predicats `dc:title` au nom visible d'objets non-pistes.
///
/// Le nom visible est celui que le DIDL met dans `<dc:title>` : le nom de
/// l'artiste, le titre de l'album, le nom du genre ou de la station. Chercher
/// sur autre chose rendrait deux resultats differents selon la rubrique.
fn retenir_par_titre<T>(
    items: Vec<T>,
    titres: &[PredicatTitre],
    nom: impl Fn(&T) -> &str,
) -> Vec<T> {
    if titres.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| titres.iter().all(|p| p.satisfait(nom(item))))
        .collect()
}

/// La page demandee et le total retenu. Meme borne de page que les pistes :
/// rendre moins que demande est permis, c'est `TotalMatches` qui dit la
/// taille reelle et un point de controle correct pagine a partir de la.
fn paginer<T>(items: Vec<T>, start: u64, count: u64) -> (Vec<T>, u64) {
    const MAX_PAGE: usize = 500;
    let total = items.len();
    let debut = usize::try_from(start).unwrap_or(usize::MAX).min(total);
    let demande = usize::try_from(count).unwrap_or(usize::MAX).min(MAX_PAGE);
    let fin = debut.saturating_add(demande).min(total);
    let page = items.into_iter().skip(debut).take(fin - debut).collect();
    (page, total as u64)
}

/// Toute la bibliotheque ne passe pas en memoire : on demande d'abord a la
/// base les titres qui PEUVENT convenir, puis on applique les predicats
/// exacts. Un critere qui n'a aucun predicat positif ne reduit rien — il
/// faudrait lire la table entiere pour le satisfaire, ce qu'on refuse (708).
fn candidats_par_titre(state: &UpnpState, titres: &[PredicatTitre]) -> Option<Vec<Track>> {
    /// Assez large pour une bibliotheque reelle, assez borne pour qu'un
    /// critere d'un seul caractere ne batisse pas un DIDL de plusieurs
    /// megaoctets. `TotalMatches` reflete ce qui a ete retenu.
    const MAX_CANDIDATS: i64 = 10_000;
    let prefiltre = titres.iter().find_map(|p| p.valeur_prefiltrante())?;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .search_by_title(prefiltre, MAX_CANDIDATS)
        .ok()?;
    Some(filtrer_par_titre(tracks, titres))
}

fn filtrer_par_titre(tracks: Vec<Track>, titres: &[PredicatTitre]) -> Vec<Track> {
    if titres.is_empty() {
        return tracks;
    }
    tracks
        .into_iter()
        .filter(|t| titres.iter().all(|p| p.satisfait(&t.title)))
        .collect()
}

fn paginate_track_results(
    tracks: Vec<Track>,
    parent_id: &str,
    start: u64,
    count: u64,
    base_url: &str,
) -> DidlResult {
    const MAX_PAGE: usize = 500;
    let total = tracks.len();
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(total);
    let requested = usize::try_from(count).unwrap_or(usize::MAX).min(MAX_PAGE);
    let end = start.saturating_add(requested).min(total);
    let page = &tracks[start..end];
    let mut inner = String::new();
    for track in page {
        inner.push_str(&didl_track_item(track, parent_id, base_url));
    }
    DidlResult {
        xml: didl_wrap(&inner),
        total: total as u64,
        returned: page.len() as u64,
    }
}

/// Les arguments de `Search` : meme forme que `Browse`, avec `ContainerID` et
/// `SearchCriteria` a la place d'`ObjectID` et `BrowseFlag`.
fn parse_search_request(soap_xml: &str) -> (String, String, u64, u64, String) {
    let mut container_id = "0".to_string();
    let mut criteria = String::new();
    let mut start: u64 = 0;
    let mut count: u64 = 100;
    let mut sort_criteria = String::new();

    let mut reader = quick_xml::Reader::from_str(soap_xml);
    reader.config_mut().trim_text(true);
    let mut current_tag = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_tag = name.rsplit(':').next().unwrap_or(&name).to_string();
                if current_tag == "SearchCriteria" {
                    // `read_text` conserve le contenu brut entier, entites
                    // comprises. Le lire d'un bloc evite que `&quot;` soit
                    // emis comme GeneralRef et coupe le critere en morceaux.
                    criteria = match reader.read_text(e.name()) {
                        Ok(raw) => {
                            let decoded = raw.decode().unwrap_or_default().into_owned();
                            match quick_xml::escape::unescape(&decoded) {
                                Ok(unescaped) => unescaped.into_owned(),
                                Err(_) => decoded,
                            }
                        }
                        Err(_) => String::new(),
                    };
                    current_tag.clear();
                }
            }
            Ok(Event::End(_)) => current_tag.clear(),
            Ok(Event::Text(e)) => {
                let v = e.decode().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "ContainerID" => container_id = v,
                    "StartingIndex" => start = v.parse().unwrap_or(0),
                    // `RequestedCount = 0` veut dire « tout », comme pour
                    // Browse : le rendre litteralement donnerait zero piste et
                    // un client conclurait a une bibliotheque vide.
                    "RequestedCount" => {
                        let n: u64 = v.parse().unwrap_or(100);
                        count = if n == 0 { u64::MAX } else { n };
                    }
                    "SortCriteria" => sort_criteria = v,
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (container_id, criteria, start, count, sort_criteria)
}

fn browse_action_response(state: &UpnpState, soap_body: &str) -> String {
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
// ConnectionManager SOAP response builders
// ---------------------------------------------------------------------------

/// Formats que le RENDERER accepte en entrée, pour le `Sink` de
/// `GetProtocolInfo`.
///
/// Un point de contrôle qui respecte la norme lit ce champ pour choisir le
/// format à nous envoyer ; un `Sink` vide ne lui laisse aucun choix valide et
/// il abandonne avant même d'essayer (Lyrion/squeeze2upnp : « no matching
/// codec p », `STMf`, lecture bloquée à 0:00 — Yacine, 22/08/2026).
///
/// La liste suit ce que la chaîne de décodage sait réellement lire — un
/// `SetAVTransportURI` traverse le même chemin qu'un flux de media server
/// externe. Les deux orthographes des types historiquement divergents sont
/// données (`audio/flac` ET `audio/x-flac`, comme pour la sonde côté client,
/// #435) : un contrôleur qui ne cherche que la sienne trouve la sienne.
///
/// `audio/L16` en est DÉLIBÉRÉMENT absent. C'est du PCM sans en-tête : rien ne
/// permet d'en deviner la cadence ni la profondeur à la lecture, et notre
/// probe travaille sur conteneur. L'annoncer ne ferait que déplacer l'échec
/// du choix du format vers le décodage — un contrôleur qui voulait du PCM
/// prendra `audio/wav`, qui porte les mêmes octets avec son en-tête.
const RENDERER_SINK_PROTOCOL_INFO: &str = concat!(
    "http-get:*:audio/flac:*,",
    "http-get:*:audio/x-flac:*,",
    "http-get:*:audio/wav:*,",
    "http-get:*:audio/x-wav:*,",
    "http-get:*:audio/wave:*,",
    "http-get:*:audio/mpeg:*,",
    "http-get:*:audio/mp3:*,",
    "http-get:*:audio/aac:*,",
    "http-get:*:audio/x-aac:*,",
    "http-get:*:audio/mp4:*,",
    "http-get:*:audio/m4a:*,",
    "http-get:*:audio/x-m4a:*,",
    "http-get:*:audio/ogg:*,",
    "http-get:*:application/ogg:*,",
    "http-get:*:audio/opus:*",
);

/// ConnectionManager du **MediaRenderer** (`/upnp/renderer/{zone}/`).
///
/// Le renderer répondait avec le ConnectionManager du media server : `Source`
/// rempli de ce qu'un SERVEUR sert, et `Sink` vide. Pour un renderer c'est
/// l'exact contraire de ce que dit la norme — `Sink` = ce qu'on sait recevoir,
/// `Source` = rien, on ne sert pas de contenu. Le symptôme se voyait jusque
/// dans nos propres journaux (`renderer_caps_probe_empty_sink`) : le client
/// DLNA de Tune sondait le renderer de Tune et n'en tirait rien.
pub fn build_renderer_connection_manager_response(soap_body: &str) -> String {
    debug!(
        body_len = soap_body.len(),
        "upnp_renderer_connection_manager_request"
    );

    match parse_soap_action(soap_body).as_deref() {
        None | Some("GetProtocolInfo") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetProtocolInfo",
            &format!("<Source></Source><Sink>{RENDERER_SINK_PROTOCOL_INFO}</Sink>"),
        ),
        Some("GetCurrentConnectionIDs") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetCurrentConnectionIDs",
            "<ConnectionIDs>0</ConnectionIDs>",
        ),
        // `Direction` est `Input` ici, et non `Output` : un renderer REÇOIT le
        // flux. La réponse partagée avec le media server annonçait `Output`,
        // soit le sens inverse du seul appareil concerne.
        Some("GetCurrentConnectionInfo") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetCurrentConnectionInfo",
            "<RcsID>0</RcsID><AVTransportID>0</AVTransportID><ProtocolInfo></ProtocolInfo><PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID><Direction>Input</Direction><Status>OK</Status>",
        ),
        Some(other) => {
            debug!(
                action = other,
                "upnp_renderer_connection_manager_unsupported_action"
            );
            soap_fault(401, "Invalid Action")
        }
    }
}

/// ConnectionManager du **MediaServer** (`/upnp/`).
pub fn build_connection_manager_response(soap_body: &str) -> String {
    debug!(
        body_len = soap_body.len(),
        "upnp_connection_manager_request"
    );

    match parse_soap_action(soap_body).as_deref() {
        // Comportement historique conservé pour un corps non identifiable.
        None | Some("GetProtocolInfo") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetProtocolInfo",
            "<Source>http-get:*:audio/flac:*,http-get:*:audio/wav:*,http-get:*:audio/mpeg:*,http-get:*:audio/ogg:*,http-get:*:audio/aac:*,http-get:*:audio/mp4:*,http-get:*:audio/x-aiff:*</Source><Sink></Sink>",
        ),
        // CM:1 impose ces deux actions ; certains points de contrôle les
        // appellent avant le premier Browse. Réponse statique : un media
        // server http-get n'entretient pas de connexions explicites,
        // l'ID 0 permanent est la réponse canonique de la spec.
        Some("GetCurrentConnectionIDs") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetCurrentConnectionIDs",
            "<ConnectionIDs>0</ConnectionIDs>",
        ),
        Some("GetCurrentConnectionInfo") => soap_action_response(
            CONNECTION_MANAGER_URN,
            "GetCurrentConnectionInfo",
            "<RcsID>-1</RcsID><AVTransportID>-1</AVTransportID><ProtocolInfo></ProtocolInfo><PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID><Direction>Output</Direction><Status>OK</Status>",
        ),
        Some(other) => {
            debug!(action = other, "upnp_connection_manager_unsupported_action");
            soap_fault(401, "Invalid Action")
        }
    }
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
    // BrowseMetadata = les métadonnées de L'OBJET lui-même, pas ses enfants.
    // L'ancienne version renvoyait « les enfants, limités à 1 » : un point de
    // contrôle strict qui valide la racine par BrowseMetadata("0") recevait
    // un enfant à la place du conteneur racine et refusait le serveur.
    let container = match object_id {
        "0" => Some(didl_container(
            "0",
            "-1",
            "Tune",
            "object.container.storageFolder",
            Some(ROOT_CONTAINERS.len() as u64),
        )),
        // Les sept rubriques racine se décrivent depuis `ROOT_CONTAINERS`,
        // seule source de vérité de leur identifiant, de leur titre et de leur
        // classe — sept branches recopiées à la main finissaient toujours par
        // diverger de la liste. Et chacune annonce le nombre d'enfants que son
        // Browse ouvrira : c'est ce que le point de contrôle affiche AVANT
        // d'ouvrir, la différence entre une bibliothèque et un dossier.
        id if ROOT_CONTAINERS.iter().any(|(racine, _, _)| *racine == id) => ROOT_CONTAINERS
            .iter()
            .find(|(racine, _, _)| *racine == id)
            .map(|(racine, titre, classe)| {
                didl_container(
                    racine,
                    "0",
                    titre,
                    classe,
                    compter_enfants_racine(state, racine),
                )
            }),
        id if id.starts_with("artist/") => {
            let artist_id: i64 = id
                .strip_prefix("artist/")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            ArtistRepo::with_backend(state.backend.clone())
                .get(artist_id)
                .ok()
                .flatten()
                .map(|a| {
                    // Décrit avec le même nombre d'albums que la liste
                    // d'artistes en annonce : deux vues du même artiste ne
                    // doivent pas donner deux tailles.
                    let nb = AlbumRepo::with_backend(state.backend.clone())
                        .count_by_artists(&[artist_id])
                        .unwrap_or_default()
                        .get(&artist_id)
                        .copied()
                        .unwrap_or(0);
                    didl_container(
                        id,
                        "artists",
                        &a.name,
                        "object.container.person.musicArtist",
                        u64::try_from(nb).ok(),
                    )
                })
        }
        id if id.starts_with("album/") => {
            let album_id: i64 = id
                .strip_prefix("album/")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            AlbumRepo::with_backend(state.backend.clone())
                .get(album_id)
                .ok()
                .flatten()
                .map(|al| {
                    didl_container(
                        id,
                        "albums",
                        &al.title,
                        "object.container.album.musicAlbum",
                        al.track_count.map(|c| c as u64),
                    )
                })
        }
        // Un genre est un conteneur comme un autre : sans cette branche, un
        // point de contrôle strict qui décrit l'objet avant de l'ouvrir
        // n'obtenait rien et abandonnait la navigation.
        id if id.starts_with("genre/") => decode_genre_id(id).map(|genre| {
            didl_container(
                id,
                "genres",
                &genre,
                "object.container.genre.musicGenre",
                None,
            )
        }),
        // Même règle que `genre/` : une année est un conteneur comme un autre,
        // et un point de contrôle strict le décrit avant de l'ouvrir.
        id if id.starts_with("year/") => decode_year_id(id)
            .map(|annee| didl_container(id, "years", &annee.to_string(), "object.container", None)),
        // Une liste de lecture se décrit avec son nombre RÉEL de pistes : c'est
        // ce `childCount` que le point de contrôle affiche avant d'ouvrir. Une
        // liste vide n'est pas publiée par `browse_playlists` — on ne la décrit
        // donc pas non plus, sinon le contrôleur ouvrirait un dossier qu'il ne
        // pouvait pas voir et le lirait comme cassé.
        id if id.starts_with("playlist/") => decode_playlist_id(id)
            .and_then(|pid| {
                PlaylistRepo::with_backend(state.backend.clone())
                    .get(pid)
                    .ok()
                    .flatten()
            })
            .filter(|liste| liste.track_count > 0)
            .map(|liste| {
                didl_container(
                    id,
                    "playlists",
                    &liste.name,
                    "object.container.playlistContainer",
                    Some(liste.track_count as u64),
                )
            }),
        _ => None,
    };

    match container {
        Some(xml) => DidlResult {
            xml: didl_wrap(&xml),
            total: 1,
            returned: 1,
        },
        None => DidlResult {
            xml: didl_wrap(""),
            total: 0,
            returned: 0,
        },
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
        "years" => browse_years(state),
        "tracks" => browse_all_tracks(state, start, count, &base_url),
        "radios" => browse_radios(state),
        "playlists" => browse_playlists(state, start, count),
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
        // `browse_genres` publiait des conteneurs `genre/<nom>` que PERSONNE
        // ne savait ouvrir : la branche manquait ici, tout genre tombait dans
        // le bras par défaut et rendait un DIDL vide (#1736, Jean Valjean).
        id if id.starts_with("genre/") => match decode_genre_id(id) {
            Some(genre) => browse_genre_albums(state, &genre, &base_url),
            None => empty_didl(),
        },
        // La leçon de #1736 vaut pour les années : un conteneur publié par
        // `browse_years` doit savoir s'ouvrir ici, sinon il se lit comme vide.
        id if id.starts_with("year/") => match decode_year_id(id) {
            Some(annee) => browse_year_albums(state, annee, &base_url),
            None => empty_didl(),
        },
        // La même leçon, pour les listes de lecture (#1802) : c'est ici que
        // « Playlists » manquait avant 0.9.79, et le conteneur se lisait comme
        // un dossier vide.
        id if id.starts_with("playlist/") => match decode_playlist_id(id) {
            Some(playlist_id) => {
                browse_playlist_tracks(state, playlist_id, start, count, &base_url)
            }
            None => empty_didl(),
        },
        _ => empty_didl(),
    }
}

/// Le DIDL des conteneurs racine. Cette liste est la SEULE source de vérité :
/// `browse_metadata("0")` en tire son nombre d'enfants, et un conteneur
/// annoncé ici doit être navigable dans `browse_direct_children` — un dossier
/// visible et vide se lit comme une bibliothèque cassée, pas comme une
/// fonction manquante.
const ROOT_CONTAINERS: [(&str, &str, &str); 7] = [
    ("artists", "Artists", "object.container"),
    ("albums", "Albums", "object.container"),
    ("genres", "Genres", "object.container"),
    // Années des albums (#1789, Jean Valjean, fil forum #1439) : chaque année
    // ouvre sur ses albums, exactement comme un genre ouvre sur les siens.
    ("years", "Years", "object.container"),
    // Parcours à plat de toute la bibliothèque. Attendu par les points de
    // contrôle — le titre de #1390 le nomme explicitement (« Albums / All
    // tracks / Genres ») — et il manquait, sans qu'aucun ticket ne le suive.
    ("tracks", "All Tracks", "object.container"),
    ("radios", "Radio", "object.container"),
    // Listes de lecture (#1802, Jean Valjean, fil forum #1439). Le conteneur
    // avait été RETIRÉ en 0.9.79 (#1758) parce que `browse_playlists` était un
    // marque-page — « Placeholder — playlists browsing can be extended later »
    // — et rendait un DIDL vide. Il revient en DERNIER, une fois
    // `browse_playlists` et `browse_playlist_tracks` écrits et testés : la
    // règle n'était pas « pas de playlists », c'était « pas de dossier vide ».
    ("playlists", "Playlists", "object.container"),
];

fn empty_didl() -> DidlResult {
    DidlResult {
        xml: didl_wrap(""),
        total: 0,
        returned: 0,
    }
}

/// Décode l'identifiant d'un conteneur de genre.
///
/// `browse_genres` encode le nom avec `urlencoding::encode`, qui échappe aussi
/// la barre oblique — « Rock/Pop » devient `genre/Rock%2FPop`. Le découpage se
/// fait donc sur le PREMIER `/` seulement, et le décodage est symétrique de
/// l'encodage : sans cela, un genre composé serait tronqué et ne retrouverait
/// jamais ses albums.
fn decode_genre_id(object_id: &str) -> Option<String> {
    let raw = object_id.strip_prefix("genre/")?;
    let decoded = urlencoding::decode(raw).ok()?.into_owned();
    if decoded.trim().is_empty() {
        return None;
    }
    Some(decoded)
}

/// Décode l'identifiant d'un conteneur d'année (`year/1959`).
///
/// Une année est un entier strictement positif : `browse_years` n'en publie
/// pas d'autre, et tout identifiant malformé rend `None` — donc un DIDL vide,
/// jamais une erreur.
fn decode_year_id(object_id: &str) -> Option<i64> {
    object_id
        .strip_prefix("year/")?
        .parse::<i64>()
        .ok()
        .filter(|annee| *annee > 0)
}

/// Décode l'identifiant d'un conteneur de liste de lecture (`playlist/12`).
///
/// Même forme que `decode_year_id` : un entier strictement positif, et tout le
/// reste rend `None` — donc un DIDL vide, jamais une erreur.
fn decode_playlist_id(object_id: &str) -> Option<i64> {
    object_id
        .strip_prefix("playlist/")?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

/// Profil dont le serveur média publie les listes de lecture.
///
/// `PlaylistRepo::list` est cloisonné par profil, mais une requête UPnP n'a ni
/// session ni en-tête : il n'y a personne à qui demander « quel profil ? ».
/// C'est le profil par défaut — le même que celui sous lequel le scan importe
/// les listes trouvées sur le disque (`library::playlist_scan`,
/// `library::folder_playlists`), donc celui qui contient réellement quelque
/// chose sur une installation ordinaire.
const UPNP_PROFILE_ID: i64 = 1;

/// Borne haute de la lecture des listes de lecture.
///
/// Le filtrage des listes vides se fait après coup : la requête doit donc
/// ramener bien plus que ce qu'une page rendra. Dix mille listes tiennent
/// largement au-delà de toute bibliothèque réelle, et bornent la mémoire.
const PLAYLIST_FETCH_CAP: i64 = 10_000;

/// Les listes de lecture du profil par défaut, triées par nom.
///
/// **Une liste vide n'est pas publiée.** C'est la règle de `ROOT_CONTAINERS`
/// descendue d'un niveau, la même que pour les années : un dossier visible et
/// vide se lit comme une bibliothèque cassée. Une liste vidée depuis
/// l'interface web disparaît donc du serveur média, et y réapparaît dès
/// qu'elle a une piste — plutôt qu'un dossier qu'on ouvre pour rien.
///
/// `total` est le nombre RÉEL de listes publiables, pas la taille de la page :
/// c'est lui que le point de contrôle lit pour savoir s'il reste des pages.
fn browse_playlists(state: &UpnpState, start: u64, count: u64) -> DidlResult {
    let listes = lire_listes_publiables(state);

    let total = listes.len();
    let debut = usize::try_from(start).unwrap_or(usize::MAX).min(total);
    let demande = usize::try_from(count).unwrap_or(usize::MAX);
    let fin = debut.saturating_add(demande).min(total);
    let page = &listes[debut..fin];

    let mut inner = String::new();
    for liste in page {
        inner.push_str(&didl_container(
            &format!("playlist/{}", liste.id.unwrap_or(0)),
            "playlists",
            &liste.name,
            "object.container.playlistContainer",
            Some(liste.track_count as u64),
        ));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total: total as u64,
        returned: page.len() as u64,
    }
}

/// Les listes de lecture que « Playlists » publie réellement : celles qui ont
/// au moins une piste et une identité. Extraite de `browse_playlists` pour que
/// le `childCount` du conteneur racine et la liste qu'il ouvre appliquent le
/// MÊME filtre — annoncer 9 listes et n'en ouvrir que 7 serait pire que de
/// n'annoncer aucun nombre.
fn lire_listes_publiables(state: &UpnpState) -> Vec<crate::db::playlist_repo::Playlist> {
    PlaylistRepo::with_backend(state.backend.clone())
        .list(UPNP_PROFILE_ID, PLAYLIST_FETCH_CAP, 0)
        .unwrap_or_default()
        .into_iter()
        .filter(|liste| liste.track_count > 0 && liste.id.is_some())
        .collect()
}

/// Les pistes d'une liste de lecture, **dans l'ordre de la liste**.
///
/// C'est le seul intérêt d'une liste de lecture, et c'est aussi le piège :
/// `playlist_tracks.position` porte l'ordre voulu, mais `list_by_ids` rend les
/// pistes dans l'ordre où la base les a écrites. Sans le ré-ordonnancement de
/// [`pistes_dans_l_ordre`], le serveur média publierait une liste dans l'ordre
/// d'insertion en base — c'est-à-dire n'importe lequel.
///
/// La page est bornée comme celle de `browse_all_tracks` : `RequestedCount=0`
/// veut dire « tout », et « tout » sur une liste de plusieurs milliers de
/// pistes bâtirait un DIDL de plusieurs mégaoctets. `TotalMatches` dit la
/// taille réelle, un point de contrôle correct pagine à partir de là.
fn browse_playlist_tracks(
    state: &UpnpState,
    playlist_id: i64,
    start: u64,
    count: u64,
    base_url: &str,
) -> DidlResult {
    const MAX_PAGE: u64 = 500;

    let ids = PlaylistRepo::with_backend(state.backend.clone())
        .get_track_ids(playlist_id)
        .unwrap_or_default();

    let total = ids.len();
    let debut = usize::try_from(start).unwrap_or(usize::MAX).min(total);
    let demande = usize::try_from(count.min(MAX_PAGE)).unwrap_or(usize::MAX);
    let fin = debut.saturating_add(demande).min(total);
    let tracks = pistes_dans_l_ordre(state, &ids[debut..fin]);

    let parent_id = format!("playlist/{playlist_id}");
    let mut inner = String::new();
    for track in &tracks {
        inner.push_str(&didl_track_item(track, &parent_id, base_url));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total: total as u64,
        returned: tracks.len() as u64,
    }
}

/// Les pistes désignées par `ids`, dans l'ordre de `ids`.
///
/// `TrackRepo::list_by_ids` bâtit un `WHERE id IN (…)`, qui n'ordonne rien.
/// Une piste absente (supprimée de la bibliothèque sans que la liste ait été
/// nettoyée) est simplement sautée ; une piste répétée dans la liste est rendue
/// autant de fois qu'elle y figure — une liste de lecture a le droit de jouer
/// deux fois le même titre.
fn pistes_dans_l_ordre(state: &UpnpState, ids: &[i64]) -> Vec<Track> {
    if ids.is_empty() {
        return Vec::new();
    }
    let par_id: std::collections::HashMap<i64, Track> =
        TrackRepo::with_backend(state.backend.clone())
            .list_by_ids(ids)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|track| track.id.map(|id| (id, track)))
            .collect();

    ids.iter()
        .filter_map(|id| par_id.get(id).cloned())
        .collect()
}

/// Le nombre d'enfants d'un conteneur RACINE, calculé sans bâtir leur DIDL.
///
/// C'est ce qui sépare une bibliothèque d'un dossier : sans `childCount`, la
/// racine d'un serveur Tune distant se lit comme sept dossiers anonymes, et
/// c'est exactement le reproche de #2299. L'attribut est standard — tout point
/// de contrôle le rend déjà, et la vue « Serveurs multimédia » aussi
/// (`network.rs::parse_didl_browse_response` le lit en `child_count`). Aucun
/// écran n'est à dessiner pour que ce nombre s'affiche.
///
/// Chaque branche appelle la MÊME source que le `browse_*` correspondant —
/// `lire_genres`, `lire_annees` et `lire_listes_publiables` ont été extraites
/// pour ça. Le test [`le_nombre_annonce_est_celui_qui_s_ouvre`] verrouille
/// l'égalité avec `browse_direct_children(..., 0, 0).total` pour chaque entrée
/// de [`ROOT_CONTAINERS`] : un compteur qui diverge de ce que le conteneur
/// ouvre serait pire que pas de compteur du tout.
///
/// `None` pour un identifiant qui n'est pas une racine : l'appelant n'émet
/// alors aucun attribut, plutôt qu'un zéro qui se lirait « dossier vide ».
fn compter_enfants_racine(state: &UpnpState, object_id: &str) -> Option<u64> {
    let n: i64 = match object_id {
        "artists" => ArtistRepo::with_backend(state.backend.clone())
            .count()
            .ok()?,
        "albums" => AlbumRepo::with_backend(state.backend.clone())
            .count()
            .ok()?,
        "genres" => lire_genres(state).len() as i64,
        "years" => lire_annees(state).len() as i64,
        "tracks" => TrackRepo::with_backend(state.backend.clone())
            .count()
            .ok()?,
        "radios" => RadioRepo::with_backend(state.backend.clone())
            .list()
            .ok()?
            .len() as i64,
        "playlists" => lire_listes_publiables(state).len() as i64,
        _ => return None,
    };
    u64::try_from(n).ok()
}

fn browse_root(state: &UpnpState) -> DidlResult {
    let containers = ROOT_CONTAINERS;

    let mut inner = String::new();
    for (id, title, class) in &containers {
        inner.push_str(&didl_container(
            id,
            "0",
            title,
            class,
            compter_enfants_racine(state, id),
        ));
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
    didl_artistes(state, &artists, "artists", total)
}

/// Le DIDL d'une liste d'artistes. `Browse` et `Search` passent tous deux par
/// ici : deux vues d'un meme artiste doivent dire exactement la meme chose.
///
/// Chaque artiste annonce son nombre d'albums — celui que `browse_artist_albums`
/// ouvrira, puisque [`AlbumRepo::count_by_artists`] applique le prédicat de
/// `list_by_artist`. Les comptes de toute la page viennent d'UNE requête : une
/// par artiste ferait cinq cents allers-retours sur une seule page de Browse.
fn didl_artistes(
    state: &UpnpState,
    artists: &[crate::db::models::Artist],
    parent_id: &str,
    total: u64,
) -> DidlResult {
    let ids: Vec<i64> = artists.iter().filter_map(|a| a.id).collect();
    let nb_albums = AlbumRepo::with_backend(state.backend.clone())
        .count_by_artists(&ids)
        .unwrap_or_default();

    let mut inner = String::new();
    for artist in artists {
        let id = format!("artist/{}", artist.id.unwrap_or(0));
        // Un artiste absent de la réponse groupée n'a aucun album visible :
        // c'est un zéro, pas une inconnue. On l'annonce, sinon un dossier sans
        // attribut se lit comme un dossier dont on ignore la taille.
        let nb = artist
            .id
            .and_then(|aid| nb_albums.get(&aid).copied())
            .unwrap_or(0);
        inner.push_str(&didl_container(
            &id,
            parent_id,
            &artist.name,
            "object.container.person.musicArtist",
            u64::try_from(nb).ok(),
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

    // `didl_albums_under` emet exactement ce conteneur — createur, pochette,
    // nombre de pistes. Seul le total differe : ici c'est la table entiere,
    // pas la page, pour que le point de controle sache qu'il reste des pages.
    let mut didl = didl_albums_under(&albums, "albums", &state.base_url());
    didl.total = total;
    didl
}

/// Toute la bibliothèque à plat, paginée.
///
/// Manquait alors que le titre de #1390 le nomme — un point de contrôle qui
/// cherche « All tracks » ne trouvait rien. Les hiérarchies Artists / Albums /
/// Genres supposent des métadonnées propres ; une liste à plat reste utile
/// quand elles ne le sont pas, et c'est souvent le seul moyen de retrouver une
/// piste mal étiquetée.
///
/// ## La pagination n'est pas une politesse, c'est la condition
///
/// La bibliothèque de référence compte 2 222 albums, donc des dizaines de
/// milliers de pistes. Rendre l'ensemble d'un coup produirait un DIDL de
/// plusieurs dizaines de mégaoctets : le point de contrôle expirerait, et le
/// conteneur se lirait comme cassé — exactement ce que le commentaire de
/// `ROOT_CONTAINERS` interdit.
///
/// `start` et `count` viennent de `StartingIndex` / `RequestedCount` et sont
/// passés tels quels à la base, qui trie de façon stable (artiste, album,
/// disque, piste). Un tri stable est indispensable : sans lui, deux pages
/// successives pourraient se recouvrir ou sauter des pistes.
///
/// `total` est le compte RÉEL de la table, pas le nombre rendu — c'est lui que
/// le point de contrôle utilise pour savoir qu'il reste des pages.
fn browse_all_tracks(state: &UpnpState, start: u64, count: u64, base_url: &str) -> DidlResult {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let total = repo.count().unwrap_or(0) as u64;
    // Borne de page, propre à ce conteneur.
    //
    // `RequestedCount = 0` signifie « tout » et devient ici
    // `UNLIMITED_BROWSE_COUNT` (100 millions). Ça convient aux albums — 2 222
    // sur la bibliothèque de référence — mais pas aux pistes : le même geste
    // sur des dizaines de milliers de titres bâtirait un DIDL de plusieurs
    // mégaoctets en mémoire, et le point de contrôle expirerait avant de le
    // recevoir. Un conteneur qui met vingt secondes à ne rien rendre se lit
    // comme cassé, ce que `ROOT_CONTAINERS` interdit explicitement.
    //
    // Rendre moins que demandé est permis par la spécification : c'est
    // `TotalMatches` qui dit la taille réelle, et un point de contrôle correct
    // pagine à partir de là. On préfère donc une première page immédiate à une
    // réponse complète qui n'arrive jamais.
    const MAX_PAGE: u64 = 500;
    let count = count.min(MAX_PAGE);
    let tracks = repo.list(count as i64, start as i64).unwrap_or_default();

    let mut inner = String::new();
    for track in &tracks {
        inner.push_str(&didl_track_item(track, "tracks", base_url));
    }

    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: tracks.len() as u64,
    }
}

fn browse_genres(state: &UpnpState) -> DidlResult {
    didl_genres(&lire_genres(state))
}

/// Le DIDL d'une liste de genres. `Browse` et `Search` passent par ici.
fn didl_genres(genres: &[String]) -> DidlResult {
    let mut inner = String::new();
    for genre in genres {
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

fn lire_genres(state: &UpnpState) -> Vec<String> {
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
    genres
}

/// Les albums d'un genre.
///
/// `AlbumRepo::list_by_genre` gère seule les genres composés (« Jazz; Blues »,
/// « Rock/Pop ») et la colonne JSON `genres` — la même correspondance que
/// l'interface web, pour que les deux vues d'un même genre donnent la même
/// liste.
fn browse_genre_albums(state: &UpnpState, genre: &str, base_url: &str) -> DidlResult {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let albums = repo.list_by_genre(genre).unwrap_or_default();
    let parent_id = format!("genre/{}", urlencoding::encode(genre));
    didl_albums_under(&albums, &parent_id, base_url)
}

/// Les années DISTINCT des albums, la plus récente d'abord.
///
/// Une année sans albums n'existe simplement pas dans la liste — la règle de
/// `ROOT_CONTAINERS` (aucun conteneur annoncé qui s'ouvre vide) descend d'un
/// niveau. Les albums sans année (`NULL` ou 0) restent visibles par les
/// autres conteneurs, mais aucun dossier « année inconnue » n'est inventé.
fn browse_years(state: &UpnpState) -> DidlResult {
    let years = lire_annees(state);

    let mut inner = String::new();
    for annee in &years {
        inner.push_str(&didl_container(
            &format!("year/{annee}"),
            "years",
            &annee.to_string(),
            "object.container",
            None,
        ));
    }

    let total = years.len() as u64;
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: total,
    }
}

/// Les années publiées par « Years ». Extraite de `browse_years` pour que le
/// `childCount` du conteneur racine et la liste qu'il ouvre viennent de la
/// MÊME requête, et ne puissent pas diverger.
fn lire_annees(state: &UpnpState) -> Vec<i64> {
    state
        .backend
        .query_many(
            "SELECT DISTINCT year FROM albums \
             WHERE year IS NOT NULL AND year > 0 ORDER BY year DESC",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.as_i64()))
        .collect()
}

/// Les albums d'une année — le même rendu que ceux d'un genre.
fn browse_year_albums(state: &UpnpState, annee: i64, base_url: &str) -> DidlResult {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let albums = repo.list_by_year(annee).unwrap_or_default();
    didl_albums_under(&albums, &format!("year/{annee}"), base_url)
}

/// Le DIDL d'une liste d'albums sous un conteneur (genre, année) : créateur,
/// pochette et nombre de pistes — pour que deux vues d'un même album disent
/// exactement la même chose.
fn didl_albums_under(
    albums: &[crate::db::models::Album],
    parent_id: &str,
    base_url: &str,
) -> DidlResult {
    let mut inner = String::new();
    for album in albums {
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
            extra.push_str(&format!(
                "<upnp:albumArtURI>{}</upnp:albumArtURI>",
                quick_xml::escape::escape(&artwork_url(base_url, cover))
            ));
        }
        inner.push_str(&didl_container_ext(
            &id,
            parent_id,
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

fn browse_radios(state: &UpnpState) -> DidlResult {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let stations = repo.list().unwrap_or_default();
    didl_radios(&stations, &state.base_url())
}

/// Le DIDL d'une liste de stations. `Browse` et `Search` passent par ici.
fn didl_radios(stations: &[crate::db::radio_repo::RadioStation], base: &str) -> DidlResult {
    let mut inner = String::new();
    for station in stations {
        let id = format!("radio/{}", station.id.unwrap_or(0));
        let mut res = String::new();
        let url = radio_audio_url(base, station.id.unwrap_or(0));
        res.push_str(&format!(
            "<res protocolInfo=\"http-get:*:audio/wav:*\">{url}</res>",
            url = quick_xml::escape::escape(&url),
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

    /// « Toutes les pistes » borne sa page même quand le point de contrôle
    /// demande tout (`RequestedCount = 0` → `UNLIMITED_BROWSE_COUNT`).
    ///
    /// Sans cette borne, une bibliothèque de plusieurs dizaines de milliers de
    /// titres bâtirait un DIDL de plusieurs mégaoctets : le point de contrôle
    /// expire, et le conteneur se lit comme cassé.
    #[test]
    fn toutes_les_pistes_borne_sa_page() {
        let source = include_str!("upnp_server.rs");
        let debut = source
            .find("fn browse_all_tracks(")
            .expect("browse_all_tracks a disparu ou a été renommée");
        let corps = &source[debut..debut + 2000];
        assert!(
            corps.contains("count.min(MAX_PAGE)"),
            "browse_all_tracks ne borne plus sa page : un RequestedCount=0 sur \
             une grosse bibliothèque rendrait un DIDL de plusieurs Mo."
        );
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

    fn album_with_genre(title: &str, genre: &str) -> crate::db::models::Album {
        crate::db::models::Album {
            id: None,
            title: title.into(),
            artist_id: None,
            artist_name: None,
            year: None,
            original_year: None,
            genre: Some(genre.into()),
            genres: None,
            disc_count: None,
            track_count: Some(9),
            cover_path: None,
            source: "local".into(),
            source_id: None,
            label: None,
            catalog_number: None,
            barcode: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            bio: None,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            release_date: None,
            original_date: None,
            added_at: None,
            is_compilation: false,
        }
    }

    /// Un état avec un vrai schéma et trois albums, dont un genre composé.
    fn state_with_albums() -> UpnpState {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = AlbumRepo::with_backend(backend.clone());
        repo.create(&album_with_genre("Kind of Blue", "Jazz"))
            .unwrap();
        repo.create(&album_with_genre("Bitches Brew", "Jazz; Fusion"))
            .unwrap();
        repo.create(&album_with_genre("The Wall", "Rock")).unwrap();
        UpnpState::new(backend, 8888, None)
    }

    /// #1736 — le cœur du défaut : `browse_genres` publiait des conteneurs
    /// `genre/<nom>` que `browse_direct_children` ne savait pas ouvrir.
    #[test]
    fn browser_un_genre_renvoie_ses_albums() {
        let state = state_with_albums();
        let res = browse_direct_children(&state, "genre/Jazz", 0, 100);
        assert_eq!(
            res.total, 2,
            "Jazz doit rendre ses deux albums, dont celui au genre composé"
        );
        assert!(res.xml.contains("Kind of Blue"));
        assert!(res.xml.contains("Bitches Brew"));
        assert!(
            !res.xml.contains("The Wall"),
            "un album d'un autre genre n'a rien à faire ici"
        );
        assert!(
            res.xml.contains("parentID=\"genre/Jazz\""),
            "le parentID doit ramener au conteneur de genre, pas à la racine"
        );
    }

    /// Contre-échec : sans la branche `genre/`, ce test rendait 0 — c'est
    /// exactement ce que Jean Valjean voyait à l'écran.
    #[test]
    fn un_genre_inconnu_reste_vide_sans_planter() {
        let state = state_with_albums();
        assert_eq!(
            browse_direct_children(&state, "genre/Zouk", 0, 100).total,
            0
        );
        assert_eq!(browse_direct_children(&state, "genre/", 0, 100).total, 0);
    }

    /// L'encodage échappe la barre oblique ; le décodage doit la rendre,
    /// sinon « Rock/Pop » serait tronqué en « Rock » et ne trouverait rien.
    #[test]
    fn l_identifiant_de_genre_se_decode_symetriquement() {
        for genre in [
            "Jazz",
            "Rock/Pop",
            "Musique française",
            "R&B",
            "Jazz; Fusion",
        ] {
            let id = format!("genre/{}", urlencoding::encode(genre));
            assert_eq!(decode_genre_id(&id).as_deref(), Some(genre), "{genre}");
        }
        assert_eq!(decode_genre_id("albums"), None);
        assert_eq!(decode_genre_id("genre/"), None);
        assert_eq!(decode_genre_id("genre/%20%20"), None);
    }

    /// Un état avec quatre albums : deux de 1959, un de 1970, un sans année.
    fn state_with_years() -> UpnpState {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = AlbumRepo::with_backend(backend.clone());
        for (titre, annee) in [
            ("Kind of Blue", Some(1959)),
            ("Time Out", Some(1959)),
            ("Bitches Brew", Some(1970)),
            ("Sans Année", None),
        ] {
            let mut album = album_with_genre(titre, "Jazz");
            album.year = annee;
            repo.create(&album).unwrap();
        }
        UpnpState::new(backend, 8888, None)
    }

    /// #1789 — le conteneur racine « Years » liste les années DISTINCT,
    /// la plus récente d'abord, et n'invente pas de dossier pour les albums
    /// sans année.
    #[test]
    fn browser_les_annees_liste_les_annees_distinctes() {
        let state = state_with_years();
        let res = browse_direct_children(&state, "years", 0, 100);
        assert_eq!(res.total, 2, "deux années distinctes : 1959 et 1970");
        assert!(res.xml.contains("id=\"year/1959\""), "{}", res.xml);
        assert!(res.xml.contains("id=\"year/1970\""), "{}", res.xml);
        let pos_1970 = res.xml.find("year/1970").unwrap();
        let pos_1959 = res.xml.find("year/1959").unwrap();
        assert!(pos_1970 < pos_1959, "la plus récente d'abord : {}", res.xml);
        assert!(res.xml.contains("parentID=\"years\""), "{}", res.xml);
    }

    /// #1789 — une année s'ouvre sur SES albums, comme un genre sur les siens.
    #[test]
    fn browser_une_annee_renvoie_ses_albums() {
        let state = state_with_years();
        let res = browse_direct_children(&state, "year/1959", 0, 100);
        assert_eq!(res.total, 2);
        assert!(res.xml.contains("Kind of Blue"), "{}", res.xml);
        assert!(res.xml.contains("Time Out"), "{}", res.xml);
        assert!(
            !res.xml.contains("Bitches Brew"),
            "un album d'une autre année n'a rien à faire ici"
        );
        assert!(
            res.xml.contains("parentID=\"year/1959\""),
            "le parentID doit ramener au conteneur d'année : {}",
            res.xml
        );
    }

    /// Contre-échec, même leçon que #1736 : un identifiant d'année inconnu ou
    /// malformé rend un DIDL vide, jamais une erreur.
    #[test]
    fn une_annee_inconnue_reste_vide_sans_planter() {
        let state = state_with_years();
        for id in ["year/1234", "year/", "year/abc", "year/-5", "year/0"] {
            assert_eq!(browse_direct_children(&state, id, 0, 100).total, 0, "{id}");
        }
    }

    /// Un point de contrôle strict décrit l'objet avant de l'ouvrir — la
    /// branche `year/` de BrowseMetadata, symétrique de celle de `genre/`.
    #[test]
    fn browse_metadata_decrit_le_conteneur_d_annee() {
        let state = state_with_years();
        let res = browse_metadata(&state, "year/1959");
        assert_eq!(res.total, 1);
        assert!(res.xml.contains("parentID=\"years\""), "{}", res.xml);
        assert!(res.xml.contains("1959"), "{}", res.xml);

        assert_eq!(browse_metadata(&state, "year/abc").total, 0);
    }

    /// `Search` sur le conteneur synthétique `years` est le même parcours à
    /// plat que sur la racine — pas un 710.
    #[test]
    fn la_recherche_accepte_le_conteneur_years() {
        let state = state_with_years();
        assert!(
            search_tracks_in_container(&state, "years", 0, 100, "http://127.0.0.1:8888", &[])
                .is_some(),
            "years est un conteneur racine annoncé : Search doit l'accepter"
        );
    }

    // -----------------------------------------------------------------------
    // #1802 — le conteneur « Playlists », cette fois peuplé
    // -----------------------------------------------------------------------

    /// Une bibliothèque avec de quoi remplir CHAQUE conteneur racine : un
    /// artiste, un album (avec genre et année), deux pistes, une radio, et
    /// deux listes de lecture dont une vide.
    ///
    /// Les identifiants rendus sont, dans l'ordre : la liste « Soirée » (trois
    /// entrées, la piste « Blue in Green » deux fois), la liste vide, et les
    /// identifiants des deux pistes.
    fn state_complet() -> (UpnpState, i64, i64, i64, i64) {
        use crate::db::models::{Album, Artist};
        use crate::db::radio_repo::RadioStation;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let artist_id = ArtistRepo::with_backend(backend.clone())
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();

        let mut album = Album::new("Kind of Blue".into());
        album.genre = Some("Jazz".into());
        album.year = Some(1959);
        album.artist_id = Some(artist_id);
        album.artist_name = Some("Miles Davis".into());
        let album_id = AlbumRepo::with_backend(backend.clone())
            .create(&album)
            .unwrap();

        let track_repo = TrackRepo::with_backend(backend.clone());
        let mut so_what = Track::new("So What".into());
        so_what.album_id = Some(album_id);
        so_what.album_title = Some("Kind of Blue".into());
        so_what.artist_id = Some(artist_id);
        so_what.artist_name = Some("Miles Davis".into());
        so_what.file_path = Some("/music/so-what.flac".into());
        let so_what_id = track_repo.create(&so_what).unwrap();

        let mut blue = Track::new("Blue in Green".into());
        blue.album_id = Some(album_id);
        blue.album_title = Some("Kind of Blue".into());
        blue.artist_id = Some(artist_id);
        blue.artist_name = Some("Miles Davis".into());
        blue.file_path = Some("/music/blue-in-green.flac".into());
        let blue_id = track_repo.create(&blue).unwrap();

        RadioRepo::with_backend(backend.clone())
            .create(&RadioStation {
                id: None,
                name: "FIP HiFi".into(),
                url: "https://icecast.example/fip-hifi.aac".into(),
                homepage: None,
                logo_url: None,
                country: None,
                language: None,
                genre: None,
                codec: None,
                bitrate: None,
                is_favorite: true,
                last_played: None,
                play_count: 0,
            })
            .unwrap();

        let playlist_repo = PlaylistRepo::with_backend(backend.clone());
        // L'ordre voulu est l'INVERSE de l'ordre d'insertion en base, et la
        // deuxième piste y figure deux fois : deux pièges d'un coup.
        let soiree = playlist_repo
            .create("Soirée & Cie", None, UPNP_PROFILE_ID)
            .unwrap();
        playlist_repo
            .set_tracks(soiree, &[blue_id, so_what_id, blue_id])
            .unwrap();
        let vide = playlist_repo
            .create("Liste vide", None, UPNP_PROFILE_ID)
            .unwrap();

        (
            UpnpState::new(backend, 8888, None),
            soiree,
            vide,
            so_what_id,
            blue_id,
        )
    }

    /// #2299 — « une vraie bibliothèque, pas un dossier ».
    ///
    /// Un dossier ne dit pas combien il contient ; une bibliothèque si. Chaque
    /// rubrique racine annonce donc sa taille, à la racine ET en
    /// `BrowseMetadata`, et ce nombre est EXACTEMENT celui que le conteneur
    /// ouvre : promettre 3 214 albums et en montrer 2 900 se lit comme une
    /// bibliothèque abîmée, pas comme un compteur approximatif.
    #[test]
    fn chaque_rayon_racine_annonce_la_taille_qu_il_ouvre() {
        let (state, _, _, _, _) = state_complet();
        let racine = browse_root(&state);

        for (id, titre, _) in ROOT_CONTAINERS.iter() {
            let ouvert = browse_direct_children(&state, id, 0, 0).total;
            assert!(
                ouvert > 0,
                "le rayon {id} ({titre}) s'ouvre vide sur une bibliothèque peuplée"
            );

            assert_eq!(
                compter_enfants_racine(&state, id),
                Some(ouvert),
                "le compteur du rayon {id} ({titre}) diverge de ce qu'il ouvre"
            );

            // Le nombre doit être DANS le DIDL, porté par ce conteneur-là.
            let attendu = format!("id=\"{id}\" parentID=\"0\" childCount=\"{ouvert}\"");
            assert!(
                racine.xml.contains(&attendu),
                "la racine n'annonce pas la taille du rayon {id} ({titre}) : {}",
                racine.xml
            );
            let meta = browse_metadata(&state, id);
            assert!(
                meta.xml.contains(&attendu),
                "BrowseMetadata({id}) n'annonce pas sa taille : {}",
                meta.xml
            );
        }
    }

    /// #2299 — un artiste annonce le nombre d'albums qu'il OUVRE, masqués
    /// exclus (#1391). Une liste de neuf cents noms sans compteur est un
    /// arbre de dossiers ; avec, c'est un index d'artistes.
    #[test]
    fn un_artiste_annonce_le_nombre_d_albums_qu_il_ouvre() {
        use crate::db::hidden_repo::HiddenRepo;
        use crate::db::models::Album;

        let (state, _, _, _, _) = state_complet();
        let backend = state.backend.clone();
        let artiste = ArtistRepo::with_backend(backend.clone())
            .list(10, 0)
            .unwrap()
            .into_iter()
            .next()
            .expect("state_complet crée un artiste");
        let artist_id = artiste.id.unwrap();

        let album_repo = AlbumRepo::with_backend(backend.clone());
        // Un second album visible…
        let mut sketches = Album::new("Sketches of Spain".into());
        sketches.artist_id = Some(artist_id);
        album_repo.create(&sketches).unwrap();
        // …et un troisième MASQUÉ, qui ne doit compter pour rien.
        let mut brouillon = Album::new("Brouillon".into());
        brouillon.artist_id = Some(artist_id);
        let masque = album_repo.create(&brouillon).unwrap();
        HiddenRepo::with_backend(backend.clone())
            .hide_album(masque)
            .unwrap();

        let conteneur = format!("artist/{artist_id}");
        let ouvert = browse_direct_children(&state, &conteneur, 0, 100).total;
        assert_eq!(ouvert, 2, "l'album masqué ne doit pas s'ouvrir");

        let attendu = format!("id=\"{conteneur}\" parentID=\"artists\" childCount=\"2\"");
        let liste = browse_direct_children(&state, "artists", 0, 100);
        assert!(
            liste.xml.contains(&attendu),
            "la liste d'artistes n'annonce pas les albums de {conteneur} : {}",
            liste.xml
        );
        assert!(
            browse_metadata(&state, &conteneur).xml.contains(&attendu),
            "BrowseMetadata({conteneur}) et la liste ne disent pas la même taille"
        );
    }

    /// #1802 — le conteneur racine « Playlists » est de retour, et il liste de
    /// vraies listes : `childCount` réel, classe `playlistContainer`, et la
    /// liste vide n'est pas publiée.
    #[test]
    fn le_conteneur_playlists_liste_les_listes_peuplees() {
        let (state, soiree, vide, _, _) = state_complet();

        let racine = browse_root(&state);
        assert!(racine.xml.contains("id=\"playlists\""), "{}", racine.xml);
        assert!(racine.xml.contains("Playlists"), "{}", racine.xml);

        let res = browse_direct_children(&state, "playlists", 0, 100);
        assert_eq!(res.total, 1, "seule la liste peuplée est publiée");
        assert_eq!(res.returned, 1);
        assert!(
            res.xml.contains(&format!("id=\"playlist/{soiree}\"")),
            "{}",
            res.xml
        );
        assert!(
            res.xml
                .contains("<upnp:class>object.container.playlistContainer</upnp:class>"),
            "{}",
            res.xml
        );
        assert!(
            res.xml.contains("childCount=\"3\""),
            "childCount doit être le nombre RÉEL de pistes : {}",
            res.xml
        );
        assert!(res.xml.contains("parentID=\"playlists\""), "{}", res.xml);
        // Le nom passe par l'échappement XML : « & » devient « &amp; », et
        // l'accent reste tel quel (le DIDL est de l'UTF-8).
        assert!(res.xml.contains("Soirée &amp; Cie"), "{}", res.xml);
        assert!(
            !res.xml.contains(&format!("id=\"playlist/{vide}\"")),
            "une liste vide ne se publie pas — c'est exactement le dossier vide \
             que #1758 avait retiré : {}",
            res.xml
        );
    }

    /// #1802 — le cœur de la demande : une liste s'ouvre sur SES pistes, dans
    /// SON ordre, y compris quand cet ordre contredit celui de la base et
    /// qu'un titre y figure deux fois.
    #[test]
    fn une_liste_de_lecture_rend_ses_pistes_dans_son_ordre() {
        let (state, soiree, _, so_what_id, blue_id) = state_complet();
        let res = browse_direct_children(&state, &format!("playlist/{soiree}"), 0, 100);

        assert_eq!(res.total, 3, "TotalMatches = les trois entrées de la liste");
        assert_eq!(res.returned, 3);

        let premier = res.xml.find("Blue in Green").unwrap();
        let second = res.xml.find("So What").unwrap();
        let troisieme = res.xml.rfind("Blue in Green").unwrap();
        assert!(
            premier < second && second < troisieme,
            "l'ordre de la liste (position) doit primer sur l'ordre de la base \
             (id croissant) : {}",
            res.xml
        );
        assert!(
            so_what_id < blue_id,
            "le jeu d'essai ne prouve rien si l'ordre de la liste est déjà \
             celui de la base"
        );
        assert!(
            res.xml
                .contains("<upnp:class>object.item.audioItem.musicTrack</upnp:class>"),
            "{}",
            res.xml
        );
        assert!(
            res.xml.contains(&format!("parentID=\"playlist/{soiree}\"")),
            "{}",
            res.xml
        );
    }

    /// La page suit `StartingIndex` / `RequestedCount`, et `TotalMatches` reste
    /// la taille RÉELLE de la liste — c'est lui qui dit au point de contrôle
    /// qu'il reste des pages. `RequestedCount = 0` veut dire « tout ».
    #[test]
    fn une_liste_de_lecture_pagine_sans_mentir_sur_le_total() {
        let (state, soiree, _, _, _) = state_complet();
        let id = format!("playlist/{soiree}");

        let page = browse_direct_children(&state, &id, 1, 1);
        assert_eq!(page.total, 3, "le total ne suit pas la page");
        assert_eq!(page.returned, 1);
        assert!(page.xml.contains("So What"), "{}", page.xml);

        let tout = browse_direct_children(&state, &id, 0, UNLIMITED_BROWSE_COUNT);
        assert_eq!(tout.total, 3);
        assert_eq!(tout.returned, 3, "RequestedCount=0 doit rendre tout");

        let au_dela = browse_direct_children(&state, &id, 99, 10);
        assert_eq!(au_dela.total, 3);
        assert_eq!(
            au_dela.returned, 0,
            "un StartingIndex hors borne ne plante pas"
        );
    }

    /// Un point de contrôle strict décrit l'objet avant de l'ouvrir — la
    /// branche `playlist/` de BrowseMetadata, symétrique de `genre/` et
    /// `year/`. Une liste vide n'étant pas publiée, elle n'est pas décrite.
    #[test]
    fn browse_metadata_decrit_le_conteneur_de_liste() {
        let (state, soiree, vide, _, _) = state_complet();

        let res = browse_metadata(&state, &format!("playlist/{soiree}"));
        assert_eq!(res.total, 1);
        assert!(res.xml.contains("parentID=\"playlists\""), "{}", res.xml);
        assert!(res.xml.contains("childCount=\"3\""), "{}", res.xml);
        assert!(
            res.xml
                .contains("<upnp:class>object.container.playlistContainer</upnp:class>"),
            "{}",
            res.xml
        );

        assert_eq!(
            browse_metadata(&state, &format!("playlist/{vide}")).total,
            0
        );
        assert_eq!(browse_metadata(&state, "playlists").total, 1);
    }

    /// Contre-échec, la leçon de #1736 : un identifiant de liste inconnu ou
    /// malformé rend un DIDL vide, jamais une erreur.
    #[test]
    fn une_liste_inconnue_reste_vide_sans_planter() {
        let (state, _, _, _, _) = state_complet();
        for id in [
            "playlist/999999",
            "playlist/",
            "playlist/abc",
            "playlist/-5",
            "playlist/0",
        ] {
            assert_eq!(browse_direct_children(&state, id, 0, 100).total, 0, "{id}");
            assert_eq!(browse_metadata(&state, id).total, 0, "{id}");
        }
    }

    /// `Search` accepte le conteneur synthétique `playlists` — sinon une
    /// recherche lancée depuis ce dossier rendrait un fault 710 — et une
    /// recherche DANS une liste se restreint réellement à ses pistes.
    #[test]
    fn la_recherche_accepte_le_conteneur_playlists_et_s_y_restreint() {
        let (state, soiree, _, _, _) = state_complet();
        let base = "http://127.0.0.1:8888";

        assert!(
            search_tracks_in_container(&state, "playlists", 0, 100, base, &[]).is_some(),
            "playlists est un conteneur racine annoncé : Search doit l'accepter"
        );

        let dans_la_liste =
            search_tracks_in_container(&state, &format!("playlist/{soiree}"), 0, 100, base, &[])
                .unwrap();
        assert_eq!(dans_la_liste.total, 3);
        assert!(
            dans_la_liste.xml.contains("So What"),
            "{}",
            dans_la_liste.xml
        );

        let filtre = [PredicatTitre {
            op: OpTitre::Contient,
            valeur: "so what".into(),
        }];
        let restreint = search_tracks_in_container(
            &state,
            &format!("playlist/{soiree}"),
            0,
            100,
            base,
            &filtre,
        )
        .unwrap();
        assert_eq!(restreint.total, 1, "{}", restreint.xml);
        assert!(
            !restreint.xml.contains("Blue in Green"),
            "{}",
            restreint.xml
        );
    }

    /// Un point de contrôle strict décrit l'objet avant de l'ouvrir.
    #[test]
    fn browse_metadata_decrit_le_conteneur_de_genre() {
        let state = state_with_albums();
        let res = browse_metadata(&state, "genre/Jazz");
        assert_eq!(res.total, 1);
        assert!(res.xml.contains("parentID=\"genres\""));
        assert!(res.xml.contains("object.container.genre.musicGenre"));
        assert!(res.xml.contains("Jazz"));
    }

    /// La racine ne doit annoncer que des dossiers réellement navigables.
    ///
    /// La version d'origine de ce test interdisait le mot « Playlists » — le
    /// conteneur venait d'être retiré en 0.9.79 parce que `browse_playlists`
    /// était un marque-page qui rendait le vide. L'intention était juste, la
    /// formulation trop courte : elle interdisait la fonctionnalité au lieu du
    /// dossier vide. Elle est donc reformulée sur ce qui compte réellement —
    /// **sur une bibliothèque peuplée, aucun conteneur racine ne s'ouvre
    /// vide**. Un marque-page échoue à ce test ; une implémentation le passe
    /// (#1802).
    #[test]
    fn la_racine_n_annonce_aucun_conteneur_impossible_a_ouvrir() {
        let (state, _, _, _, _) = state_complet();
        let root = browse_root(&state);
        assert_eq!(root.total, ROOT_CONTAINERS.len() as u64);

        // Et le nombre d'enfants annoncé par BrowseMetadata suit la liste.
        let meta = browse_metadata(&state, "0");
        assert!(
            meta.xml
                .contains(&format!("childCount=\"{}\"", ROOT_CONTAINERS.len()))
        );

        for (id, titre, _) in ROOT_CONTAINERS.iter() {
            // Chaque conteneur racine annoncé doit savoir se décrire…
            assert!(
                browse_metadata(&state, id).total == 1,
                "conteneur racine {id} sans BrowseMetadata"
            );
            // …être annoncé à la racine…
            assert!(
                root.xml.contains(&format!("id=\"{id}\"")),
                "conteneur racine {id} ({titre}) absent de la racine : {}",
                root.xml
            );
            // …et s'ouvrir sur quelque chose.
            let enfants = browse_direct_children(&state, id, 0, 100);
            assert!(
                enfants.total > 0 && enfants.returned > 0,
                "le conteneur racine {id} ({titre}) s'ouvre vide sur une \
                 bibliothèque peuplée : un dossier visible et vide se lit comme \
                 une bibliothèque cassée"
            );
        }
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
    fn le_media_server_publie_une_url_tune_wav_sans_contacter_la_station() {
        use crate::db::radio_repo::RadioStation;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let station_id = RadioRepo::with_backend(backend.clone())
            .create(&RadioStation {
                id: None,
                name: "FIP HiFi".into(),
                url: "https://icecast.example/fip-hifi.aac".into(),
                homepage: None,
                logo_url: None,
                country: Some("France".into()),
                language: None,
                genre: None,
                codec: None,
                bitrate: None,
                is_favorite: true,
                last_played: None,
                play_count: 0,
            })
            .unwrap();
        let mut state = UpnpState::new(backend, 8888, None);
        state.advertised_ip = Some("192.168.1.18".into());

        let didl = browse_radios(&state);
        let url = radio_audio_url(&state.base_url(), station_id);

        assert!(didl.total >= 1);
        assert!(didl.xml.contains(&format!(
            "protocolInfo=\"http-get:*:audio/wav:*\">{url}</res>"
        )));
        assert!(
            !didl.xml.contains("icecast.example"),
            "Browse divulgue encore l URL externe et contourne Tune : {}",
            didl.xml
        );
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
        // CM:1 impose aussi ces deux actions — des points de contrôle stricts
        // les appellent avant le premier Browse.
        assert!(scpd.contains("<name>GetCurrentConnectionIDs</name>"));
        assert!(scpd.contains("<name>GetCurrentConnectionInfo</name>"));
    }

    fn soap_body(action: &str, urn: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:{action} xmlns:u="{urn}"></u:{action}></s:Body>
</s:Envelope>"#
        )
    }

    #[test]
    fn parse_soap_action_extrait_le_nom() {
        let body = soap_body(
            "GetSortCapabilities",
            "urn:schemas-upnp-org:service:ContentDirectory:1",
        );
        assert_eq!(
            parse_soap_action(&body).as_deref(),
            Some("GetSortCapabilities")
        );
        assert_eq!(parse_soap_action("pas du xml"), None);
    }

    #[test]
    fn content_directory_dispatch_repond_a_chaque_action() {
        // Avant : TOUTE action recevait une BrowseResponse — un client strict
        // qui appelle GetSortCapabilities avant son premier Browse (JPLAY)
        // recevait un corps invalide et abandonnait.
        let state = test_state();
        let urn = "urn:schemas-upnp-org:service:ContentDirectory:1";

        let sort = build_browse_response(&state, &soap_body("GetSortCapabilities", urn));
        assert!(sort.contains("<u:GetSortCapabilitiesResponse"));
        assert!(!sort.contains("BrowseResponse"));

        let search = build_browse_response(&state, &soap_body("GetSearchCapabilities", urn));
        assert!(search.contains("<u:GetSearchCapabilitiesResponse"));

        let update = build_browse_response(&state, &soap_body("GetSystemUpdateID", urn));
        assert!(update.contains("<u:GetSystemUpdateIDResponse"));
        assert!(update.contains("<Id>1</Id>"));

        // `Search` REPOND desormais (#1516) : elle est declaree au SCPD et
        // servie. Ce test attendait un 401 — il encodait l'ancien
        // comportement comme voulu, et c'est justement lui qui laissait les
        // clients d'indexation (JPlay) sans recours.
        // `Search` exige ses arguments. Un corps sans `SearchCriteria` doit
        // désormais recevoir 708 : l'ancien test appelait donc lui-même une
        // action invalide et masquait le contrat strict que nous annonçons.
        let search_body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:Search xmlns:u="{urn}">
    <ContainerID>0</ContainerID>
    <SearchCriteria>upnp:class derivedfrom &quot;object.item.audioItem&quot;</SearchCriteria>
    <Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>1</RequestedCount>
    <SortCriteria></SortCriteria>
  </u:Search></s:Body>
</s:Envelope>"#
        );
        let search_resp = build_browse_response(&state, &search_body);
        assert!(search_resp.contains("<u:SearchResponse"), "{search_resp}");
        assert!(!is_soap_fault(&search_resp));

        // Une action vraiment non declaree rend toujours un fault 401 : le
        // dispatch n'est pas devenu laxiste.
        let fault = build_browse_response(&state, &soap_body("CreateObject", urn));
        assert!(is_soap_fault(&fault));
        assert!(fault.contains("<errorCode>401</errorCode>"));

        // Un Browse ordinaire répond toujours en BrowseResponse.
        let browse = build_browse_response(&state, &soap_body("Browse", urn));
        assert!(browse.contains("<u:BrowseResponse"));
    }

    #[derive(Clone, Default)]
    struct JournalCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl JournalCapture {
        fn texte(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for JournalCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
        type Writer = JournalCapture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn un_refus_content_directory_nomme_action_et_objet_sans_accuser_les_actions_valides() {
        let state = test_state();
        let urn = "urn:schemas-upnp-org:service:ContentDirectory:1";
        let journal = JournalCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let with_object = format!(
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body><u:CreateObject xmlns:u="{urn}"><ObjectID>albums/42</ObjectID></u:CreateObject></s:Body>
</s:Envelope>"#
            );
            assert!(is_soap_fault(&build_browse_response(&state, &with_object)));
            assert!(is_soap_fault(&build_browse_response(
                &state,
                &soap_body("DestroyObject", urn)
            )));
            assert!(!is_soap_fault(&build_browse_response(
                &state,
                &soap_body("GetSystemUpdateID", urn)
            )));
        });

        let log = journal.texte();
        assert!(
            log.contains("WARN"),
            "le refus doit survivre au niveau par défaut: {log}"
        );
        assert!(log.contains("action=\"CreateObject\""), "{log}");
        assert!(log.contains("object_id=\"albums/42\""), "{log}");
        assert!(log.contains("action=\"DestroyObject\""), "{log}");
        assert!(log.contains("object_id=\"<absent>\""), "{log}");
        assert_eq!(
            log.matches("upnp_content_directory_unsupported_action")
                .count(),
            2,
            "une action valide ne doit produire aucun faux refus: {log}"
        );
    }

    #[test]
    fn connection_manager_dispatch_actions_obligatoires() {
        let urn = "urn:schemas-upnp-org:service:ConnectionManager:1";

        let ids = build_connection_manager_response(&soap_body("GetCurrentConnectionIDs", urn));
        assert!(ids.contains("<u:GetCurrentConnectionIDsResponse"));
        assert!(ids.contains("<ConnectionIDs>0</ConnectionIDs>"));

        let info = build_connection_manager_response(&soap_body("GetCurrentConnectionInfo", urn));
        assert!(info.contains("<u:GetCurrentConnectionInfoResponse"));
        assert!(info.contains("<Direction>Output</Direction>"));
        assert!(info.contains("<Status>OK</Status>"));

        let proto = build_connection_manager_response(&soap_body("GetProtocolInfo", urn));
        assert!(proto.contains("<u:GetProtocolInfoResponse"));

        let fault = build_connection_manager_response(&soap_body("PrepareForConnection", urn));
        assert!(is_soap_fault(&fault));
    }

    #[test]
    fn le_renderer_annonce_ce_qu_il_sait_recevoir() {
        // La régression : le renderer répondait avec le ConnectionManager du
        // media server — `Source` rempli, `Sink` VIDE. Un point de contrôle
        // normé n'a alors aucun format valide à nous envoyer et abandonne
        // avant d'essayer (Lyrion/squeeze2upnp : « no matching codec »,
        // lecture bloquée à 0:00). Le sens des deux champs est inversé entre
        // un serveur et un renderer, ils ne peuvent pas partager la réponse.
        let urn = "urn:schemas-upnp-org:service:ConnectionManager:1";
        let proto = build_renderer_connection_manager_response(&soap_body("GetProtocolInfo", urn));

        assert!(proto.contains("<u:GetProtocolInfoResponse"));
        assert!(
            !proto.contains("<Sink></Sink>"),
            "un renderer qui n'annonce aucun format d'entrée est injouable"
        );
        assert!(
            proto.contains("<Source></Source>"),
            "un renderer ne sert rien"
        );
        // Les formats que le contrôleur a le plus de chances de vouloir.
        for mime in [
            "audio/flac",
            "audio/x-flac",
            "audio/wav",
            "audio/mpeg",
            "audio/mp4",
            "audio/ogg",
        ] {
            assert!(
                proto.contains(&format!("http-get:*:{mime}:*")),
                "{mime} absent du Sink"
            );
        }

        // Et le media server garde EXACTEMENT l'inverse : c'est lui qui sert.
        let serveur = build_connection_manager_response(&soap_body("GetProtocolInfo", urn));
        assert!(serveur.contains("<Sink></Sink>"));
        assert!(serveur.contains("<Source>http-get:"));
    }

    #[test]
    fn le_renderer_recoit_le_flux_il_ne_l_emet_pas() {
        // `Direction` venait de la réponse du media server : `Output`, soit le
        // sens inverse du seul appareil concerné.
        let urn = "urn:schemas-upnp-org:service:ConnectionManager:1";
        let info =
            build_renderer_connection_manager_response(&soap_body("GetCurrentConnectionInfo", urn));
        assert!(info.contains("<Direction>Input</Direction>"));
        assert!(info.contains("<Status>OK</Status>"));

        let ids =
            build_renderer_connection_manager_response(&soap_body("GetCurrentConnectionIDs", urn));
        assert!(ids.contains("<ConnectionIDs>0</ConnectionIDs>"));

        let fault =
            build_renderer_connection_manager_response(&soap_body("PrepareForConnection", urn));
        assert!(is_soap_fault(&fault));
    }

    #[test]
    fn le_sink_du_renderer_ne_promet_pas_du_pcm_sans_en_tete() {
        // `audio/L16` est du PCM nu : ni cadence ni profondeur lisibles au fil
        // de l'eau, et notre probe travaille sur conteneur. L'annoncer
        // déplacerait l'échec du choix du format vers le décodage, ce qui est
        // pire — le contrôleur croirait avoir négocié.
        let urn = "urn:schemas-upnp-org:service:ConnectionManager:1";
        let proto = build_renderer_connection_manager_response(&soap_body("GetProtocolInfo", urn));
        assert!(!proto.contains("audio/L16"));
    }

    #[test]
    fn browse_metadata_racine_renvoie_le_conteneur_lui_meme() {
        // BrowseMetadata("0") doit décrire la racine (parentID -1), pas son
        // premier enfant — c'est ainsi qu'un client strict valide le serveur.
        let state = test_state();
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
    <ObjectID>0</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag>
    <StartingIndex>0</StartingIndex><RequestedCount>1</RequestedCount>
  </u:Browse></s:Body>
</s:Envelope>"#;
        let resp = build_browse_response(&state, body);
        // Le DIDL est échappé dans <Result> — on vérifie sur la forme échappée.
        assert!(resp.contains("container id=&quot;0&quot; parentID=&quot;-1&quot;"));
        assert!(resp.contains("<TotalMatches>1</TotalMatches>"));
    }

    #[test]
    fn description_expose_x_dlnadoc_et_serial() {
        let state = test_state();
        let xml = build_device_description(&state);
        assert!(xml.contains("<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>"));
        assert!(xml.contains("xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\""));
        assert!(xml.contains("<serialNumber>"));
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

    fn search_test_state() -> UpnpState {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        UpnpState::new(Arc::new(db), 8888, None)
    }

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

    // --- L'action Search, pour les clients qui INDEXENT (#1516) ---

    fn soap_search(container: &str, criteria: &str, start: u64, count: u64) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope><s:Body>
<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ContainerID>{container}</ContainerID>
<SearchCriteria>{criteria}</SearchCriteria>
<StartingIndex>{start}</StartingIndex>
<RequestedCount>{count}</RequestedCount>
</u:Search></s:Body></s:Envelope>"#
        )
    }

    #[test]
    fn les_arguments_de_search_sont_lus() {
        let (c, crit, start, count, sort) = parse_search_request(&soap_search(
            "0",
            "upnp:class derivedfrom &quot;object.item.audioItem&quot;",
            50,
            25,
        ));
        assert_eq!(c, "0");
        assert_eq!(
            crit, "upnp:class derivedfrom \"object.item.audioItem\"",
            "les entites XML doivent etre resolues avant l'evaluation"
        );
        assert_eq!(start, 50);
        assert_eq!(count, 25);
        assert!(sort.is_empty());
    }

    /// Meme regle que pour Browse : `RequestedCount = 0` veut dire « tout ».
    /// Le rendre litteralement donnerait zero piste, et le client conclurait a
    /// une bibliotheque vide — ce qui ressemble exactement au defaut qu'on
    /// corrige.
    #[test]
    fn requested_count_zero_veut_dire_tout() {
        let (_, _, _, count, _) = parse_search_request(&soap_search("0", "*", 0, 0));
        assert_eq!(count, u64::MAX);
    }

    #[test]
    fn un_critere_de_pistes_est_reconnu_sous_ses_formes_usuelles() {
        for c in [
            "upnp:class derivedfrom \"object.item.audioItem\"",
            "upnp:class = \"object.item.audioItem.musicTrack\"",
            "*",
        ] {
            assert!(
                evaluate_supported_class_criteria(c).unwrap(),
                "devrait viser des pistes : {c}"
            );
        }
    }

    /// Un client qui cherche des PHOTOS ne doit pas recevoir nos morceaux.
    /// Sans ce garde, « object.item.imageItem » passerait par la clause
    /// `object.item` et rendrait toute la discotheque.
    #[test]
    fn une_recherche_d_images_ou_de_videos_ne_rend_rien() {
        for c in [
            "upnp:class derivedfrom \"object.item.imageItem\"",
            "upnp:class derivedfrom \"object.item.videoItem.movie\"",
        ] {
            assert_eq!(
                evaluate_supported_class_criteria(c),
                Ok(false),
                "ne doit rien rendre : {c}"
            );
        }
    }

    #[test]
    fn un_predicat_non_annonce_est_refuse_au_lieu_de_tout_rendre() {
        // `dc:title` est desormais annonce ET evalue : ce n'est plus lui
        // l'exemple du champ inconnu. L'intention du test — refuser plutot
        // que rendre toute la bibliotheque — est reportee sur `upnp:artist`,
        // qui reste hors de SEARCH_CAPS.
        for c in [
            "upnp:artist contains \"Introuvable\"",
            "upnp:class derivedfrom \"object.item.audioItem\" and upnp:artist contains \"Introuvable\"",
            "",
        ] {
            assert_eq!(evaluer_criteres(c).err(), Some(()), "{c}");
        }

        let state = search_test_state();
        let response = search_action_response(
            &state,
            &soap_search(
                "0",
                "upnp:class derivedfrom &quot;object.item.audioItem&quot; and upnp:artist contains &quot;Introuvable&quot;",
                0,
                100,
            ),
        );
        assert!(
            response.contains("<errorCode>708</errorCode>"),
            "{response}"
        );
        assert!(!response.contains("<u:SearchResponse"), "{response}");
    }

    #[test]
    fn un_tri_non_annonce_est_refuse() {
        let state = search_test_state();
        let soap = soap_search(
            "0",
            "upnp:class derivedfrom &quot;object.item.audioItem&quot;",
            0,
            100,
        )
        .replace(
            "</u:Search>",
            "<SortCriteria>+dc:title</SortCriteria></u:Search>",
        );
        let response = search_action_response(&state, &soap);
        assert!(
            response.contains("<errorCode>709</errorCode>"),
            "{response}"
        );
    }

    #[test]
    fn search_respecte_le_conteneur_album_et_son_total() {
        use crate::db::models::Artist;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let artist_repo = ArtistRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend.clone());
        let track_repo = TrackRepo::with_backend(backend.clone());

        let artist_id = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let mut blue = crate::db::models::Album::new("Kind of Blue".into());
        blue.genre = Some("Jazz".into());
        blue.artist_id = Some(artist_id);
        blue.artist_name = Some("Miles Davis".into());
        let blue_id = album_repo.create(&blue).unwrap();
        let mut wall = crate::db::models::Album::new("The Wall".into());
        wall.genre = Some("Rock".into());
        wall.artist_id = Some(artist_id);
        wall.artist_name = Some("Miles Davis".into());
        let wall_id = album_repo.create(&wall).unwrap();

        let mut so_what = Track::new("So What".into());
        so_what.album_id = Some(blue_id);
        so_what.album_title = Some("Kind of Blue".into());
        so_what.artist_id = Some(artist_id);
        so_what.artist_name = Some("Miles Davis".into());
        so_what.file_path = Some("/music/so-what.flac".into());
        track_repo.create(&so_what).unwrap();

        let mut money = Track::new("Money".into());
        money.album_id = Some(wall_id);
        money.album_title = Some("The Wall".into());
        money.artist_id = Some(artist_id);
        money.artist_name = Some("Miles Davis".into());
        money.file_path = Some("/music/money.flac".into());
        track_repo.create(&money).unwrap();

        let state = UpnpState::new(backend, 8888, None);
        let result = search_tracks_in_container(
            &state,
            &format!("album/{blue_id}"),
            0,
            100,
            "http://127.0.0.1:8888",
            &[],
        )
        .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.returned, 1);
        assert!(result.xml.contains("So What"), "{}", result.xml);
        assert!(!result.xml.contains("Money"), "{}", result.xml);
    }

    /// Une action sans argument s'ecrit legitimement en element vide. Le
    /// dispatcher ne lisait que `Event::Start` : il rendait alors `None`, et
    /// le repli historique repondait une BrowseResponse — la racine du
    /// serveur, presentee comme des capacites de recherche. Constate en direct
    /// sur .18 en 0.9.103.
    #[test]
    fn une_action_auto_fermante_est_reconnue() {
        let corps = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body><u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/></s:Body>
</s:Envelope>"#;
        assert_eq!(
            parse_soap_action(corps).as_deref(),
            Some("GetSearchCapabilities")
        );

        let ouverte = corps.replace(
            r#"<u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/>"#,
            r#"<u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"></u:GetSearchCapabilities>"#,
        );
        assert_eq!(
            parse_soap_action(&ouverte).as_deref(),
            Some("GetSearchCapabilities"),
            "les deux formes doivent dire la meme action"
        );
    }

    #[test]
    fn les_capacites_annoncees_ne_sont_plus_vides() {
        // Un SearchCaps VIDE dit « je ne sais rien chercher » — tout en
        // repondant 401 a Search. C'etait le double message qui laissait les
        // clients d'indexation sans recours.
        assert!(!SEARCH_CAPS.is_empty());
    }

    /// L'invariant de #2312, tenu dans LES DEUX SENS.
    ///
    /// Sens 1 — ne pas annoncer ce qu'on n'evalue pas : chaque champ de
    /// `SEARCH_CAPS` doit etre accepte par l'evaluateur.
    /// Sens 2 — ne pas evaluer en silence ce qu'on n'annonce pas : un champ
    /// absent de `SEARCH_CAPS` doit etre refuse.
    #[test]
    fn les_capacites_annoncees_sont_toutes_evaluees() {
        for champ in SEARCH_CAPS.split(',') {
            let critere = if champ == "upnp:class" {
                format!("{champ} = \"object.item.audioItem.musicTrack\"")
            } else {
                format!("{champ} contains \"x\"")
            };
            assert!(
                evaluer_criteres(&critere).is_ok(),
                "{champ} est annonce dans SEARCH_CAPS mais l'evaluateur le refuse"
            );
        }
        for champ in ["upnp:artist", "upnp:album", "dc:creator", "upnp:genre"] {
            assert!(
                !SEARCH_CAPS.contains(champ),
                "{champ} est annonce sans etre evalue"
            );
            assert!(
                evaluer_criteres(&format!("{champ} contains \"x\"")).is_err(),
                "{champ} n'est pas annonce, il doit etre refuse (708)"
            );
        }
    }

    #[test]
    fn un_predicat_de_titre_est_reconnu() {
        let c = evaluer_criteres("dc:title contains \"Kind of Blue\"").unwrap();
        assert_eq!(c.cible, Some(CibleRecherche::Pistes));
        assert_eq!(
            c.titres,
            vec![PredicatTitre {
                op: OpTitre::Contient,
                valeur: "Kind of Blue".into()
            }]
        );
    }

    #[test]
    fn la_conjonction_classe_et_titre_est_reconnue() {
        let c = evaluer_criteres(
            "upnp:class derivedfrom \"object.item.audioItem\" and dc:title contains \"So What\"",
        )
        .unwrap();
        assert_eq!(c.cible, Some(CibleRecherche::Pistes));
        assert_eq!(c.titres.len(), 1);
        assert_eq!(c.titres[0].valeur, "So What");
    }

    /// Un titre peut contenir « and ». Couper dessus ferait chercher un
    /// morceau de phrase, sans que rien ne le signale.
    #[test]
    fn le_and_a_l_interieur_des_guillemets_n_est_pas_un_separateur() {
        let c = evaluer_criteres("dc:title contains \"Peaches and Cream\"").unwrap();
        assert_eq!(c.titres.len(), 1);
        assert_eq!(c.titres[0].valeur, "Peaches and Cream");
    }

    // --- Le menu d'un lecteur reseau passe par Search (#1777, fil 1439) ---

    /// L'etat du releve du 30/08/2026 : un artiste, deux albums donc deux
    /// genres, une station de radio. De quoi remplir les quatre rubriques que
    /// le ND8006 voit vides.
    fn state_du_releve_nd8006() -> UpnpState {
        use crate::db::radio_repo::RadioStation;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let artiste_id = ArtistRepo::with_backend(backend.clone())
            .create(&crate::db::models::Artist::new("Miles Davis".into()))
            .unwrap();
        let albums = AlbumRepo::with_backend(backend.clone());
        let mut kind_of_blue = crate::db::models::Album::new("Kind of Blue".into());
        kind_of_blue.genre = Some("Jazz".into());
        kind_of_blue.artist_id = Some(artiste_id);
        albums.create(&kind_of_blue).unwrap();
        let mut the_wall = crate::db::models::Album::new("The Wall".into());
        the_wall.genre = Some("Rock".into());
        albums.create(&the_wall).unwrap();
        RadioRepo::with_backend(backend.clone())
            .create(&RadioStation {
                id: None,
                name: "FIP HiFi".into(),
                url: "https://icecast.example/fip-hifi.aac".into(),
                homepage: None,
                logo_url: None,
                country: None,
                language: None,
                genre: None,
                codec: None,
                bitrate: None,
                is_favorite: true,
                last_played: None,
                play_count: 0,
            })
            .unwrap();
        UpnpState::new(backend, 8888, None)
    }

    /// Le defaut du fil forum #1439, tenu sur les quatre rubriques a la fois.
    ///
    /// Jean Valjean voit, dans la MEME session sur son Marantz ND8006 :
    /// « Parcourir les dossiers » plein — Artists, Albums, Genres, jusqu'aux
    /// morceaux — et les entrees Artistes / Albums / Genres / Radios du menu
    /// de l'appareil vides. Le premier chemin est `Browse`, le second
    /// `Search`. Les deux sont ici cote a cote : ce que l'un montre, l'autre
    /// doit le trouver.
    #[test]
    fn le_menu_du_lecteur_voit_les_memes_rubriques_que_le_parcours_de_dossiers() {
        let state = state_du_releve_nd8006();
        for (classe, conteneur, attendu) in [
            (
                "object.container.person.musicArtist",
                "artists",
                "Miles Davis",
            ),
            (
                "object.container.album.musicAlbum",
                "albums",
                "Kind of Blue",
            ),
            ("object.container.genre.musicGenre", "genres", "Jazz"),
            ("object.item.audioItem.audioBroadcast", "radios", "FIP HiFi"),
        ] {
            let parcours = browse_direct_children(&state, conteneur, 0, 100);
            assert!(
                parcours.xml.contains(attendu),
                "« Parcourir les dossiers » ne montre deja pas {attendu} dans {conteneur} : {}",
                parcours.xml
            );

            let reponse = search_action_response(
                &state,
                &soap_search(
                    "0",
                    &format!("upnp:class derivedfrom &quot;{classe}&quot;"),
                    0,
                    100,
                ),
            );
            assert!(
                reponse.contains("<u:SearchResponse"),
                "{classe} : {reponse}"
            );
            assert!(
                reponse.contains(attendu),
                "le menu du lecteur lit « liste vide » pour {classe} : {reponse}"
            );
            assert!(
                !reponse.contains("<NumberReturned>0</NumberReturned>"),
                "{classe} : {reponse}"
            );
        }
    }

    /// Le garde de non-regression du parcours d'indexation (#1516).
    ///
    /// `derivedfrom "object.item.audioItem"` laisse desormais passer DEUX
    /// classes — pistes et radios. La regle est que les pistes l'emportent :
    /// un client qui indexe doit recevoir exactement ce qu'il recevait avant,
    /// sinon reparer une rubrique en casserait une autre.
    #[test]
    fn l_indexation_vise_toujours_les_pistes_et_rien_d_autre() {
        for c in ["*", "upnp:class derivedfrom \"object.item.audioItem\""] {
            assert_eq!(
                evaluer_criteres(c).unwrap().cible,
                Some(CibleRecherche::Pistes),
                "{c}"
            );
        }
    }

    /// Une classe qu'on ne publie pas, ou une expression qui en vise
    /// plusieurs, rend une liste VIDE — jamais une faute, et jamais un
    /// melange qu'aucun point de controle ne saurait paginer.
    #[test]
    fn une_classe_inconnue_ou_ambigue_rend_une_liste_vide() {
        for critere in [
            "upnp:class derivedfrom \"object.item.imageItem\"",
            "upnp:class != \"object.item.audioItem.musicTrack\"",
        ] {
            assert_eq!(evaluer_criteres(critere).unwrap().cible, None, "{critere}");
        }
    }

    /// Un predicat de titre doit mordre sur les conteneurs comme il mord sur
    /// les pistes : sinon `Search` rendrait toute la rubrique en laissant
    /// croire qu'il a cherche — la faute exacte de #2312.
    #[test]
    fn un_predicat_de_titre_filtre_aussi_les_conteneurs() {
        let state = state_du_releve_nd8006();
        let reponse = search_action_response(
            &state,
            &soap_search(
                "0",
                "upnp:class derivedfrom &quot;object.container.album.musicAlbum&quot; \
                 and dc:title contains &quot;Kind&quot;",
                0,
                100,
            ),
        );
        assert!(reponse.contains("Kind of Blue"), "{reponse}");
        assert!(!reponse.contains("The Wall"), "{reponse}");
        assert!(
            reponse.contains("<NumberReturned>1</NumberReturned>"),
            "{reponse}"
        );
    }

    /// Chercher des artistes DANS un album n'a pas de sens : la liste est
    /// vide, elle n'est pas fautive. Seul un identifiant qu'on ne publie
    /// nulle part merite le 710 — la meme distinction que pour les pistes.
    #[test]
    fn chercher_une_rubrique_hors_de_sa_portee_rend_une_liste_vide() {
        let state = state_du_releve_nd8006();
        let artiste = "upnp:class derivedfrom &quot;object.container.person.musicArtist&quot;";

        let dans_un_album =
            search_action_response(&state, &soap_search("album/1", artiste, 0, 100));
        assert!(
            dans_un_album.contains("<NumberReturned>0</NumberReturned>"),
            "{dans_un_album}"
        );
        assert!(!dans_un_album.contains("<errorCode>"), "{dans_un_album}");

        let inconnu = search_action_response(&state, &soap_search("chose/42", artiste, 0, 100));
        assert!(inconnu.contains("<errorCode>710</errorCode>"), "{inconnu}");
    }

    /// Ce qu'on ne sait pas evaluer doit etre REFUSE, pas approxime : c'est
    /// tout l'objet de #2312.
    #[test]
    fn ce_qui_n_est_pas_evalue_est_refuse() {
        for critere in [
            "dc:title contains \"a\" or dc:title contains \"b\"",
            "(dc:title contains \"a\")",
            "upnp:artist = \"Miles Davis\"",
            "dc:title exists true",
            "dc:title contains \"pas de guillemet fermant",
            "and dc:title contains \"a\"",
        ] {
            assert!(
                evaluer_criteres(critere).is_err(),
                "ce critere devait etre refuse : {critere}"
            );
        }
    }

    #[test]
    fn le_filtre_de_titre_ignore_casse_et_accents() {
        let p = PredicatTitre {
            op: OpTitre::Contient,
            valeur: "ELEGIE".into(),
        };
        assert!(p.satisfait("Élégie pour un ami"));
        assert!(!p.satisfait("Nocturne"));

        let negatif = PredicatTitre {
            op: OpTitre::NeContientPas,
            valeur: "live".into(),
        };
        assert!(negatif.satisfait("So What"));
        assert!(!negatif.satisfait("So What (Live)"));
    }

    /// Un predicat negatif ne reduit rien : il ne peut pas servir de
    /// pre-filtre SQL, sans quoi on lirait la table entiere en memoire.
    #[test]
    fn un_predicat_negatif_ne_prefiltre_pas() {
        assert_eq!(
            PredicatTitre {
                op: OpTitre::Contient,
                valeur: "blue".into()
            }
            .valeur_prefiltrante(),
            Some("blue")
        );
        assert_eq!(
            PredicatTitre {
                op: OpTitre::NeContientPas,
                valeur: "live".into()
            }
            .valeur_prefiltrante(),
            None
        );
    }

    #[test]
    fn la_recherche_par_titre_restreint_dans_un_album() {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let album_repo = AlbumRepo::with_backend(backend.clone());
        let track_repo = TrackRepo::with_backend(backend.clone());
        let blue_id = album_repo
            .create(&crate::db::models::Album::new("Kind of Blue".into()))
            .unwrap();

        for titre in ["So What", "So What (Live)", "Blue in Green"] {
            let mut t = Track::new(titre.into());
            t.album_id = Some(blue_id);
            t.album_title = Some("Kind of Blue".into());
            t.file_path = Some(format!("/music/{titre}.flac"));
            track_repo.create(&t).unwrap();
        }

        let state = UpnpState::new(backend, 8888, None);
        let criteres =
            evaluer_criteres("dc:title contains \"So What\" and dc:title doesNotContain \"Live\"")
                .unwrap();
        let result = search_tracks_in_container(
            &state,
            &format!("album/{blue_id}"),
            0,
            100,
            "http://127.0.0.1:8888",
            &criteres.titres,
        )
        .unwrap();

        assert_eq!(result.total, 1, "{}", result.xml);
        assert!(result.xml.contains("So What"), "{}", result.xml);
        assert!(!result.xml.contains("Live"), "{}", result.xml);
        assert!(!result.xml.contains("Blue in Green"), "{}", result.xml);
    }
}
