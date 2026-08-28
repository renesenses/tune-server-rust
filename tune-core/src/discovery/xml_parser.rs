use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;

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

// ── Un descriptif illisible doit se diagnostiquer sur pièces (#2665) ────────
//
// Journal de Jean Valjean (fil forum 1585, v0.9.116, 28/08/2026) :
//
//   DEBUG tune_core::discovery::xml_parser: xml_parse_error
//       error=ill-formed document: expected `</meta>`, but `</head>` was found
//
// Un `</meta>` fermé par un `</head>` : ce n'est pas un descriptif UPnP mal
// formé, c'est une PAGE WEB. Une adresse annoncée en SSDP rend du HTML là où
// la découverte attend une description d'appareil — box, imprimante, caméra,
// portail captif, console d'administration. Le cas est banal et n'est pas en
// soi un défaut de Tune ; le défaut est qu'on ne peut savoir NI de quel
// appareil il s'agit, NI quelle adresse ouvrir dans un navigateur pour le
// constater.
//
// #2584 a fait nommer l'adresse à la boucle de scan SSDP
// (`ssdp_device_create_failed`, `ssdp.rs`). Mais c'est UN appelant sur six, et
// l'erreur elle-même n'a jamais porté l'adresse : les erreurs HTTP de
// `fetch_device_description` l'embarquent (`HTTP fetch {location}: …`), l'erreur
// d'ANALYSE non. C'est l'asymétrie que ce module corrige ici, une fois, à
// l'endroit unique qui connaît à la fois l'adresse ET le corps reçu.

/// Ce qu'on a le droit de recopier d'un corps de réponse dans le journal.
///
/// **Jamais le corps entier.** Un journal de testeur finit sur un forum
/// public, et la page servie par le fautif est justement celle d'un
/// équipement du foyer : console de box, caméra, imprimante. Elle peut porter
/// un SSID, un nom de machine, un jeton de session dans un formulaire caché.
/// 200 octets suffisent à reconnaître `<!doctype html>`, `{"error":…}` ou un
/// `<?xml` tronqué — et la trace dit toujours combien d'octets elle a laissés
/// de côté, pour qu'on sache qu'elle n'a pas tout montré.
const BODY_EXCERPT_LIMIT: usize = 200;

/// Ce à quoi ressemble un corps que [`parse_device_description`] a refusé.
///
/// Le genre n'est pas un détail de confort : c'est lui qui dit quoi FAIRE.
/// « page HTML » envoie ouvrir l'adresse dans un navigateur pour identifier
/// l'équipement ; « XML malformé » envoie regarder le descriptif lui-même.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadableKind {
    /// Rien du tout : zéro octet, ou uniquement des blancs.
    Empty,
    /// Une page web. C'est le cas du journal de Jean Valjean.
    Html,
    /// Un objet ou un tableau JSON — une API web sur le port annoncé.
    Json,
    /// Ça commence bien comme du XML, mais il est malformé ou tronqué.
    MalformedXml,
    /// Autre chose : binaire, texte brut, protocole étranger.
    NotXml,
}

impl UnreadableKind {
    /// Formulation destinée au journal, lisible par qui n'a pas le code sous
    /// les yeux.
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "corps vide",
            Self::Html => "page HTML — un serveur web, pas un descriptif UPnP",
            Self::Json => "JSON — une API web, pas un descriptif UPnP",
            Self::MalformedXml => "XML malformé ou tronqué",
            Self::NotXml => "ni XML ni HTML",
        }
    }
}

/// Diagnostic d'un corps illisible : ce que c'était, sa taille réelle, et un
/// début borné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableBody {
    pub kind: UnreadableKind,
    /// Taille RÉELLE du corps reçu, en octets. Toujours journalisée, même
    /// quand l'extrait est mille fois plus court.
    pub total_bytes: usize,
    /// Nombre d'octets du corps que [`UnreadableBody::excerpt`] recouvre.
    pub excerpt_bytes: usize,
    /// Début du corps, assaini (tout caractère de contrôle devient une espace,
    /// pour qu'une ligne de journal reste une ligne) et borné à
    /// [`BODY_EXCERPT_LIMIT`] octets.
    pub excerpt: String,
}

impl UnreadableBody {
    /// Vrai si l'extrait ne montre pas tout — la trace doit le dire.
    pub fn truncated(&self) -> bool {
        self.excerpt_bytes < self.total_bytes
    }
}

/// Plus grand indice `<= max` qui soit une frontière de caractère.
///
/// Couper un UTF-8 au milieu paniquerait sur `&s[..max]`, et un descriptif
/// qui rend du HTML accentué est exactement le cas où ça arriverait.
fn char_boundary_at_or_before(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Reconnaît ce qu'un corps refusé était réellement.
///
/// Fonction **pure** : c'est elle qu'on teste, et rien de ce qu'elle décide ne
/// dépend d'une horloge, du réseau ou d'un état global.
pub fn describe_unreadable_body(body: &str) -> UnreadableBody {
    let debut = body.trim_start();
    let kind = if debut.is_empty() {
        UnreadableKind::Empty
    } else {
        // On ne renifle que la tête : un descriptif UPnP légitime commence par
        // `<?xml` ou `<root`, jamais par `<!doctype html`. Chercher `<head` ou
        // `<meta` dans TOUT le corps ferait passer pour du HTML un descriptif
        // qui les citerait dans un `<modelDescription>`.
        let tete = &debut[..char_boundary_at_or_before(debut, 512)].to_ascii_lowercase();
        if tete.starts_with("<!doctype html")
            || tete.starts_with("<html")
            || tete.contains("<head")
            || tete.contains("<meta")
            || tete.contains("<body")
        {
            UnreadableKind::Html
        } else if debut.starts_with('{') || debut.starts_with('[') {
            UnreadableKind::Json
        } else if debut.starts_with('<') {
            UnreadableKind::MalformedXml
        } else {
            UnreadableKind::NotXml
        }
    };
    let excerpt_bytes = char_boundary_at_or_before(body, BODY_EXCERPT_LIMIT);
    // Un caractère de contrôle vaut un octet en UTF-8 et l'espace aussi :
    // `excerpt_bytes` reste donc l'exacte quantité de corps recouverte.
    let excerpt = body[..excerpt_bytes]
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    UnreadableBody {
        kind,
        total_bytes: body.len(),
        excerpt_bytes,
        excerpt,
    }
}

/// Combien de temps une même adresse reste tue après avoir été journalisée.
///
/// Un portail captif interrogé à chaque cycle de scan (`IDLE_SCAN_INTERVAL`,
/// 120 s) produirait sinon une ligne identique toutes les deux minutes, pour
/// la vie du processus. Une trace toutes les quinze minutes suffit largement
/// à établir que l'adresse est durablement fautive, et la ligne annonce
/// combien d'occurrences elle a passées sous silence : rien n'est caché, tout
/// est compté.
const FAILURE_LOG_WINDOW: Duration = Duration::from_secs(900);

/// Plafond du nombre d'adresses suivies. Sur un réseau à baux DHCP courts, un
/// même équipement peut défiler sous beaucoup d'adresses ; la table ne doit
/// pas croître pour la vie du processus (c'est exactement le travers que
/// #2633 a corrigé sur le registre des serveurs multimédia).
const FAILURE_LOG_CAP: usize = 256;

/// Décide si une adresse fautive a droit à une ligne de journal maintenant.
#[derive(Debug, Default)]
struct FailureLog {
    /// adresse → (dernière trace émise, occurrences tues depuis)
    seen: HashMap<String, (Instant, u32)>,
}

impl FailureLog {
    /// `Some(tues)` ⇒ journaliser, en annonçant `tues` occurrences passées
    /// sous silence depuis la dernière trace. `None` ⇒ se taire.
    ///
    /// `now` est fourni par l'appelant : la décision est déterministe, donc
    /// vérifiable sans dormir quinze minutes.
    fn admit(&mut self, location: &str, now: Instant) -> Option<u32> {
        if let Some((last, suppressed)) = self.seen.get_mut(location) {
            if now.duration_since(*last) < FAILURE_LOG_WINDOW {
                *suppressed = suppressed.saturating_add(1);
                return None;
            }
            let tues = *suppressed;
            *last = now;
            *suppressed = 0;
            return Some(tues);
        }
        if self.seen.len() >= FAILURE_LOG_CAP {
            self.seen
                .retain(|_, (last, _)| now.duration_since(*last) < FAILURE_LOG_WINDOW);
            if self.seen.len() >= FAILURE_LOG_CAP {
                // Toutes les entrées sont encore fraîches : on repart de zéro
                // plutôt que de croître. Au pire, quelques doublons.
                self.seen.clear();
            }
        }
        self.seen.insert(location.to_string(), (now, 0));
        Some(0)
    }
}

static FAILURE_LOG: LazyLock<Mutex<FailureLog>> =
    LazyLock::new(|| Mutex::new(FailureLog::default()));

pub fn parse_device_description(xml: &str) -> Result<DeviceDescription, String> {
    let mut reader = Reader::from_str(xml);
    let mut device_stack: Vec<DeviceDescription> = Vec::new();
    let mut root_device: Option<DeviceDescription> = None;
    let mut embedded_devices: Vec<DeviceDescription> = Vec::new();
    let mut current_service: Option<ServiceDescription> = None;
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                current_tag = tag.clone();
                match tag.as_str() {
                    "device" => device_stack.push(DeviceDescription::default()),
                    "service" => {
                        current_service = Some(ServiceDescription::default());
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match tag.as_str() {
                    "device" => {
                        if let Some(device) = device_stack.pop() {
                            if device_stack.is_empty() && root_device.is_none() {
                                root_device = Some(device);
                            } else {
                                embedded_devices.push(device);
                            }
                        }
                    }
                    "service" => {
                        if let (Some(service), Some(device)) =
                            (current_service.take(), device_stack.last_mut())
                            && !service.service_type.is_empty()
                        {
                            device.services.push(service);
                        }
                    }
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Text(ref e)) => {
                let decoded = e.decode().unwrap_or_default();
                let text = match unescape(&decoded) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => decoded.trim().to_string(),
                };
                if text.is_empty() {
                    continue;
                }
                if let Some(service) = current_service.as_mut() {
                    match current_tag.as_str() {
                        "serviceType" => service.service_type = text,
                        "serviceId" => service.service_id = text,
                        "controlURL" => service.control_url = text,
                        "eventSubURL" => service.event_sub_url = text,
                        "SCPDURL" => service.scpd_url = text,
                        _ => {}
                    }
                } else if let Some(device) = device_stack.last_mut() {
                    match current_tag.as_str() {
                        "friendlyName" => device.friendly_name = text,
                        "manufacturer" => device.manufacturer = text,
                        "modelName" => device.model_name = text,
                        "modelDescription" => device.model_description = text,
                        "UDN" => device.udn = text,
                        "deviceType" => device.device_type = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                // Pas de `tracing` ici : cette fonction ne connaît QUE le
                // texte, pas l'adresse d'où il vient. L'ancien
                // `debug!(error = %e, "xml_parse_error")` ne portait donc
                // aucun identifiant — et, `log_level` valant `info` par
                // défaut (`tune-core/src/config.rs`), il n'apparaissait même
                // pas dans un journal ordinaire. C'est
                // `fetch_device_description`, seule à tenir l'adresse ET le
                // corps, qui journalise (#2665).
                return Err(format!("XML parse error: {e}"));
            }
            _ => {}
        }
        buf.clear();
    }

    let mut desc = root_device.unwrap_or_default();

    // Composite UPnP descriptions commonly expose a MediaRenderer and a
    // MediaServer below one root device. Flattening every service lets the
    // server's later ConnectionManager overwrite the renderer's one, whose
    // Sink is then legitimately empty (#2072). Keep the SSDP root identity,
    // but attach only the service-owning renderer. Prefer the standard device
    // type, then tolerate the same non-standard AVTransport devices accepted
    // by discovery today.
    if !desc.is_media_renderer() && !desc.has_av_transport() {
        let renderer = embedded_devices
            .iter()
            .find(|device| device.is_media_renderer())
            .or_else(|| {
                embedded_devices
                    .iter()
                    .find(|device| device.has_av_transport())
            })
            .cloned();

        if let Some(renderer) = renderer {
            if desc.friendly_name.is_empty() {
                desc.friendly_name = renderer.friendly_name.clone();
            }
            if desc.manufacturer.is_empty() {
                desc.manufacturer = renderer.manufacturer.clone();
            }
            if desc.model_name.is_empty() {
                desc.model_name = renderer.model_name.clone();
            }
            if desc.model_description.is_empty() {
                desc.model_description = renderer.model_description.clone();
            }
            // Append so a renderer service wins over a same-named root service
            // in service_urls/event_sub_urls, while retaining root-only vendor
            // services used by some OpenHome-compatible devices.
            desc.services.extend(renderer.services);
        }
    }

    Ok(desc)
}

pub async fn fetch_device_description(location: &str) -> Result<DeviceDescription, String> {
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let xml = client
        .get(location)
        .send()
        .await
        .map_err(|e| {
            let rendered = crate::http::error::chain(&e);
            crate::http::error::hint_if_local_network_denied(&rendered);
            format!("HTTP fetch {location}: {rendered}")
        })?
        .text()
        .await
        .map_err(|e| format!("HTTP body {location}: {}", crate::http::error::chain(&e)))?;

    parse_device_description(&xml).map_err(|e| {
        // Les deux erreurs HTTP ci-dessus embarquent `location` dans leur
        // message ; seule l'erreur d'ANALYSE ne l'avait pas — et c'est la
        // seule qu'on ne peut pas diagnostiquer sans elle. Six appelants
        // relaient cette chaîne, dont trois la jettent en silence
        // (`ssdp::probe_renderer`, `background::spawn_ssdp_startup_scan`,
        // `routes::devices::scan_devices`) : la trace est émise ICI, une fois,
        // pour tous (#2665).
        report_unreadable_description(location, &xml, &e);
        // L'extrait du corps reste dans le journal LOCAL et n'entre pas dans
        // la chaîne d'erreur : celle-ci remonte jusqu'aux réponses HTTP de
        // l'API (`routes/devices.rs`, ajout manuel d'un DLNA). Le genre et la
        // taille suffisent à qualifier l'échec côté client.
        format!(
            "XML parse {location}: {e} — {}, {} octets reçus",
            describe_unreadable_body(&xml).kind.label(),
            xml.len()
        )
    })
}

/// Journalise un descriptif illisible : l'adresse interrogée, ce que le corps
/// était réellement, sa taille, et un début borné.
///
/// Niveau `warn` — et non `debug` comme l'ancien `xml_parse_error` — parce que
/// `log_level` vaut `info` par défaut (`tune-core/src/config.rs:219`) : au
/// niveau `debug`, la seule trace de l'incident n'apparaissait même pas dans
/// un journal ordinaire, et l'appareil pouvait donc disparaître de la liste
/// des lecteurs sans une ligne. Le niveau ne coûte pas de bruit : un échec
/// d'ANALYSE suppose que quelque chose a bel et bien répondu avec autre chose
/// qu'un descriptif — les pannes réseau, elles, ne passent pas par ici.
fn report_unreadable_description(location: &str, body: &str, parse_error: &str) {
    let admis = match FAILURE_LOG.lock() {
        Ok(mut log) => log.admit(location, Instant::now()),
        // Verrou empoisonné : on préfère une ligne de trop au silence.
        Err(_) => Some(0),
    };
    let Some(tues) = admis else {
        return;
    };
    let diag = describe_unreadable_body(body);
    warn!(
        location = %location,
        corps = %diag.kind.label(),
        octets = diag.total_bytes,
        extrait_octets = diag.excerpt_bytes,
        tronque = diag.truncated(),
        extrait = %diag.excerpt,
        erreur = %parse_error,
        occurrences_tues = tues,
        action = "cette adresse a été annoncée en SSDP mais ne rend pas de \
                  descriptif UPnP. Ouvrez-la dans un navigateur pour identifier \
                  l'équipement : si ce n'est pas un lecteur, il n'y a rien à \
                  corriger dans Tune.",
        "upnp_description_unreadable"
    );
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

    const COMPOSITE_RENDERER_AND_SERVER_XML: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-denon-com:device:AiosDevice:1</deviceType>
    <friendlyName>Marantz ND8006</friendlyName>
    <manufacturer>Marantz</manufacturer>
    <modelName>ND8006</modelName>
    <UDN>uuid:root-advertised-by-ssdp</UDN>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
        <friendlyName>Marantz ND8006 Renderer</friendlyName>
        <UDN>uuid:embedded-renderer</UDN>
        <serviceList>
          <service>
            <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
            <controlURL>/upnp/control/renderer_dvc/AVTransport</controlURL>
            <eventSubURL>/upnp/event/renderer_dvc/AVTransport</eventSubURL>
          </service>
          <service>
            <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
            <controlURL>/upnp/control/renderer_dvc/ConnectionManager</controlURL>
            <eventSubURL>/upnp/event/renderer_dvc/ConnectionManager</eventSubURL>
          </service>
        </serviceList>
      </device>
      <device>
        <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
        <friendlyName>Marantz ND8006 Server</friendlyName>
        <UDN>uuid:embedded-server</UDN>
        <serviceList>
          <service>
            <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
            <controlURL>/upnp/control/ams_dvc/ContentDirectory</controlURL>
          </service>
          <service>
            <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
            <controlURL>/upnp/control/ams_dvc/ConnectionManager</controlURL>
            <eventSubURL>/upnp/event/ams_dvc/ConnectionManager</eventSubURL>
          </service>
        </serviceList>
      </device>
    </deviceList>
  </device>
</root>"#;

    #[test]
    fn composite_device_keeps_root_identity_and_renderer_services() {
        let desc = parse_device_description(COMPOSITE_RENDERER_AND_SERVER_XML).unwrap();

        assert_eq!(desc.udn, "uuid:root-advertised-by-ssdp");
        assert_eq!(desc.friendly_name, "Marantz ND8006");
        assert_eq!(
            desc.device_type,
            "urn:schemas-denon-com:device:AiosDevice:1"
        );
        assert!(desc.has_av_transport());

        let control_urls = desc.service_urls();
        assert_eq!(
            control_urls.get("connectionmanager").map(String::as_str),
            Some("/upnp/control/renderer_dvc/ConnectionManager")
        );
        assert_eq!(
            control_urls.get("avtransport").map(String::as_str),
            Some("/upnp/control/renderer_dvc/AVTransport")
        );
        assert!(!control_urls.contains_key("contentdirectory"));

        let event_urls = desc.event_sub_urls();
        assert_eq!(
            event_urls.get("connectionmanager").map(String::as_str),
            Some("/upnp/event/renderer_dvc/ConnectionManager")
        );
    }
}

// ── #2665 : un descriptif illisible doit se diagnostiquer sur pièces ────────
//
// Ce que le journal de Jean Valjean donnait — et tout ce qu'il donnait :
//
//   DEBUG xml_parse_error error=ill-formed document: expected `</meta>`,
//                                but `</head>` was found
//
// On sait qu'une adresse du réseau rend du HTML. On ne sait pas laquelle. On
// ne peut ni l'ouvrir, ni chercher qui l'annonce, ni dire au testeur quel
// équipement débrancher. Les tests ci-dessous fixent le contrat inverse :
// l'adresse, la nature du corps, sa taille, un début borné — et pas une ligne
// de plus qu'il n'en faut quand l'appareil s'entête.
#[cfg(test)]
mod descriptif_illisible {
    use super::*;

    /// Page d'accueil type d'une console d'équipement : des balises vides non
    /// refermées dans un `<head>`, ce qui produit la famille d'erreurs du
    /// journal de Jean Valjean (« expected `</…>`, but `</head>` was found »).
    ///
    /// Le nom de réseau est délibérément placé **au-delà du 200e octet** :
    /// c'est ce qui rend vérifiable que la troncature protège quelque chose.
    const PAGE_HTML: &str = "<!DOCTYPE html>\n<html lang=\"fr\">\n<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<link rel=\"stylesheet\" href=\"/static/admin.css\">\n\
<title>Console d'administration</title>\n</head>\n\
<body><h1>Bienvenue</h1><p>Réseau : Livebox-4F2A</p>\
<p>Adresse MAC : 00:1A:2B:3C:4D:5E</p></body></html>\n";

    /// Le marqueur sensible du fixture, et la preuve qu'il est bien hors de
    /// portée de l'extrait.
    const NOM_DE_RESEAU: &str = "Livebox-4F2A";

    #[test]
    fn le_fixture_place_bien_le_nom_de_reseau_au_dela_de_la_limite() {
        // Contre-épreuve du fixture lui-même : sans ça, les assertions
        // « l'extrait ne contient pas le nom de réseau » passeraient pour une
        // bonne raison (il est tronqué) ou pour une mauvaise (il n'y est
        // pas), et rien ne les distinguerait.
        let position = PAGE_HTML
            .find(NOM_DE_RESEAU)
            .expect("le fixture doit contenir le marqueur qu'on prétend protéger");
        assert!(
            position > BODY_EXCERPT_LIMIT,
            "le marqueur est au {position}e octet, dans les {BODY_EXCERPT_LIMIT} \
             premiers : le test ne prouverait rien"
        );
    }

    #[test]
    fn une_page_web_est_nommee_page_web_et_pas_xml_mal_forme() {
        let diag = describe_unreadable_body(PAGE_HTML);
        assert_eq!(
            diag.kind,
            UnreadableKind::Html,
            "sans ce verdict, la trace envoie chercher un descriptif fautif \
             alors qu'il n'y a aucun descriptif du tout"
        );
        assert!(
            diag.kind.label().contains("HTML"),
            "la formulation doit être lisible sans le code sous les yeux : {}",
            diag.kind.label()
        );
    }

    #[test]
    fn un_vrai_descriptif_upnp_tronque_reste_du_xml_mal_forme() {
        // Contre-épreuve du test précédent : le reniflage ne doit pas voir du
        // HTML partout. Un descriptif coupé en plein vol reste du XML.
        let tronque = "<?xml version=\"1.0\"?>\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
<device><friendlyName>Marantz ND8006</friendlyName>";
        assert_eq!(
            describe_unreadable_body(tronque).kind,
            UnreadableKind::MalformedXml
        );
    }

    #[test]
    fn le_corps_entier_n_est_jamais_recopie_et_la_trace_dit_ce_qu_elle_omet() {
        // Une page de 12 000 octets : ce qu'une console de box rend vraiment.
        let grande_page = format!("{PAGE_HTML}{}", "<p>ligne</p>".repeat(1000));
        let diag = describe_unreadable_body(&grande_page);
        assert_eq!(
            diag.total_bytes,
            grande_page.len(),
            "la taille annoncée doit être la taille RÉELLE, pas celle de l'extrait"
        );
        assert!(
            diag.excerpt.len() <= BODY_EXCERPT_LIMIT,
            "un journal de testeur part sur un forum public : {} octets recopiés",
            diag.excerpt.len()
        );
        assert_eq!(diag.excerpt_bytes, BODY_EXCERPT_LIMIT);
        assert!(
            diag.truncated(),
            "une trace qui tronque sans le dire laisse croire qu'on a tout vu"
        );
        assert!(
            !diag.excerpt.contains(NOM_DE_RESEAU),
            "le nom de réseau est au-delà de la limite : il ne doit pas sortir"
        );
        assert!(
            !diag.excerpt.contains('\n'),
            "une ligne de journal doit rester une ligne : {:?}",
            diag.excerpt
        );
        assert!(
            diag.excerpt.starts_with("<!DOCTYPE html>"),
            "l'extrait doit montrer ce qui permet de reconnaître le corps : {:?}",
            diag.excerpt
        );
    }

    #[test]
    fn la_quantite_de_corps_recopiee_reste_de_deux_cents_octets() {
        // Valeur en dur, délibérément : tous les autres tests mesurent
        // l'extrait CONTRE `BODY_EXCERPT_LIMIT`, si bien qu'élargir la
        // constante élargirait aussi leur oracle et passerait inaperçu (une
        // mutation l'a montré). Ce que la constante engage n'est pas un détail
        // d'implémentation mais une décision de confidentialité : la page
        // recopiée est celle d'un équipement du foyer, et le journal part sur
        // un forum public. La relever se discute — pas en silence.
        assert_eq!(BODY_EXCERPT_LIMIT, 200);
    }

    #[test]
    fn un_corps_plus_court_que_la_limite_n_est_pas_annonce_tronque() {
        let diag = describe_unreadable_body("<html>");
        assert_eq!(diag.total_bytes, 6);
        assert_eq!(diag.excerpt_bytes, 6);
        assert!(!diag.truncated());
    }

    #[test]
    fn l_extrait_ne_coupe_jamais_un_caractere_en_deux() {
        // 199 octets ASCII puis un « é » sur deux octets : l'octet 200 tombe
        // au MILIEU du caractère. `&body[..200]` paniquerait.
        let corps = format!("{}é", "a".repeat(BODY_EXCERPT_LIMIT - 1));
        let diag = describe_unreadable_body(&corps);
        assert_eq!(diag.total_bytes, BODY_EXCERPT_LIMIT + 1);
        assert_eq!(
            diag.excerpt_bytes,
            BODY_EXCERPT_LIMIT - 1,
            "la coupe doit reculer jusqu'à la frontière de caractère"
        );
        assert_eq!(diag.excerpt, "a".repeat(BODY_EXCERPT_LIMIT - 1));
    }

    #[test]
    fn les_autres_corps_illisibles_sont_nommes_pour_ce_qu_ils_sont() {
        assert_eq!(describe_unreadable_body("").kind, UnreadableKind::Empty);
        assert_eq!(
            describe_unreadable_body("   \n\t ").kind,
            UnreadableKind::Empty,
            "un corps de blancs est vide pour qui diagnostique"
        );
        assert_eq!(
            describe_unreadable_body("{\"error\":\"unauthorized\"}").kind,
            UnreadableKind::Json
        );
        assert_eq!(
            describe_unreadable_body("[]").kind,
            UnreadableKind::Json,
            "un tableau JSON est du JSON"
        );
        assert_eq!(
            describe_unreadable_body("\u{0}\u{1}\u{2}binaire").kind,
            UnreadableKind::NotXml
        );
    }

    // ── Ne pas inonder le journal ───────────────────────────────────────────
    //
    // Un portail captif est interrogé à chaque cycle de scan (120 s au repos)
    // et échoue à chaque fois, pour la vie du processus. Une ligne par
    // tentative rendrait le journal inexploitable — exactement ce qu'a
    // documenté le relevé des 79 lignes identiques d'affilée sur un autre
    // chemin. La règle : une ligne, puis le silence, puis une ligne qui dit
    // combien d'occurrences ont été tues.

    #[test]
    fn un_appareil_bavard_ne_produit_qu_une_ligne_par_fenetre() {
        let mut journal = FailureLog::default();
        let t0 = Instant::now();
        const ADRESSE: &str = "http://192.0.2.1:1900/rootDesc.xml";

        assert_eq!(
            journal.admit(ADRESSE, t0),
            Some(0),
            "la première occurrence doit toujours sortir"
        );
        // 78 tentatives de plus dans la fenêtre : 79 en tout, comme le relevé.
        for i in 1..79u64 {
            assert_eq!(
                journal.admit(ADRESSE, t0 + Duration::from_secs(i * 10)),
                None,
                "tentative {i} : le journal doit rester muet dans la fenêtre"
            );
        }
        assert_eq!(
            journal.admit(ADRESSE, t0 + FAILURE_LOG_WINDOW),
            Some(78),
            "la ligne suivante doit avouer les occurrences passées sous silence"
        );
        assert_eq!(
            journal.admit(ADRESSE, t0 + FAILURE_LOG_WINDOW + Duration::from_secs(1)),
            None,
            "le compteur repart à zéro, la fenêtre aussi"
        );
    }

    #[test]
    fn deux_adresses_fautives_ne_se_font_pas_taire_l_une_l_autre() {
        let mut journal = FailureLog::default();
        let t0 = Instant::now();
        assert_eq!(journal.admit("http://192.0.2.1/a.xml", t0), Some(0));
        assert_eq!(
            journal.admit("http://192.0.2.2/b.xml", t0),
            Some(0),
            "l'étranglement est par adresse : un deuxième fautif doit se voir"
        );
    }

    #[test]
    fn la_table_des_adresses_fautives_ne_croit_pas_indefiniment() {
        let mut journal = FailureLog::default();
        let t0 = Instant::now();
        for i in 0..(FAILURE_LOG_CAP * 2) {
            journal.admit(&format!("http://192.0.2.{i}/desc.xml"), t0);
        }
        assert!(
            journal.seen.len() <= FAILURE_LOG_CAP,
            "un réseau à baux DHCP courts ferait croître la table pour la vie \
             du processus — c'est le travers corrigé par #2633 ailleurs ; \
             taille observée : {}",
            journal.seen.len()
        );
    }

    #[test]
    fn les_adresses_devenues_muettes_sont_purgees_avant_de_plafonner() {
        let mut journal = FailureLog::default();
        let t0 = Instant::now();
        for i in 0..FAILURE_LOG_CAP {
            journal.admit(&format!("http://192.0.2.{i}/desc.xml"), t0);
        }
        assert_eq!(journal.seen.len(), FAILURE_LOG_CAP);
        let plus_tard = t0 + FAILURE_LOG_WINDOW + Duration::from_secs(1);
        journal.admit("http://198.51.100.7/desc.xml", plus_tard);
        assert_eq!(
            journal.seen.len(),
            1,
            "les entrées hors fenêtre doivent partir en premier, pas tout le monde"
        );
    }

    // ── De bout en bout : ce qu'on lira vraiment dans le journal ────────────

    /// Un serveur web ordinaire à l'adresse annoncée : il rend `corps` sur
    /// `chemin`, et 404 partout ailleurs.
    async fn serveur_web(chemin: &'static str, corps: &'static str) -> std::net::SocketAddr {
        // Pas d'IPv6, et un port éphémère : deux tests peuvent tourner côte à
        // côte sans se disputer une adresse.
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = ecoute.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let tete = String::from_utf8_lossy(&req);
                    let demande = tete
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let resp = if demande == chemin {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                            corps.len()
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    // De bout en bout : la chaîne d'erreur que six appelants relaient.
    //
    // Ce test ne regarde PAS le journal, et c'est délibéré. `tracing` met en
    // cache, pour tout le processus, la décision « ce point d'appel
    // intéresse-t-il quelqu'un ? » et le niveau maximal utile ; un abonné posé
    // le temps d'un `await`, dans un binaire de test qui en crée des dizaines
    // en parallèle, se voit priver d'évènements de façon imprévisible. Mesuré :
    // capture vide **1 exécution sur 6** de la suite complète du crate, alors
    // que le test seul passait 8 fois sur 8. Un test qui échoue une fois sur
    // six ne prouve rien et fait perdre son temps à tout le monde.
    //
    // La ligne de journal est donc vérifiée là où elle peut l'être sans
    // course : `tune-core/tests/journal_descriptif_illisible.rs`, un binaire de
    // test dédié qui installe un abonné GLOBAL et ne contient que ce test.
    #[tokio::test]
    async fn l_erreur_d_analyse_nomme_l_adresse_et_la_nature_du_corps() {
        const CHEMIN: &str = "/rootDesc.xml";
        let addr = serveur_web(CHEMIN, PAGE_HTML).await;
        let location = format!("http://{addr}{CHEMIN}");
        let err = fetch_device_description(&location)
            .await
            .expect_err("une page HTML ne peut pas produire un descriptif");
        // Les erreurs HTTP de cette même fonction portaient déjà l'URL ;
        // l'erreur d'analyse était la seule à ne pas l'avoir, et la seule qui
        // en avait besoin.
        assert!(
            err.contains(&location),
            "l'erreur ne nomme pas l'adresse interrogée : {err}"
        );
        assert!(
            err.contains("HTML"),
            "l'erreur ne dit pas que le corps était une page web : {err}"
        );
        assert!(
            err.contains(&format!("{} octets", PAGE_HTML.len())),
            "l'erreur ne dit pas la taille reçue : {err}"
        );
        assert!(
            !err.contains(NOM_DE_RESEAU),
            "aucun morceau du corps ne doit remonter dans une chaîne d'erreur \
             qui finit dans une réponse HTTP : {err}"
        );
    }
}
