//! Redécouverte CIBLÉE d'un renderer UPnP dont le port de contrôle a changé.
//!
//! Le cas mesuré (#3829, .42 sous Windows, v0.9.145) : un renderer à pile
//! Platinum/1.0.5.13 redémarre et tire un nouveau port au hasard (1145 →
//! 1838), comme cette pile le fait à chaque démarrage. Tune garde l'URL de
//! contrôle apprise à la première découverte et l'appelle indéfiniment ; le
//! `10061` (`ECONNREFUSED`) est le refus d'un port qui n'écoute plus. Or
//! l'appareil répond au `M-SEARCH` unicast en moins d'une seconde, avec sa
//! nouvelle `LOCATION`, et son UDN n'a pas changé.
//!
//! Ce module ne fait QUE cela : un `M-SEARCH` unicast vers l'adresse connue,
//! avec `ST: uuid:<udn>`, puis la relecture de `description.xml` pour en tirer
//! les URLs de contrôle absolues. Pas de balayage multicast — il réveillerait
//! toutes les zones et coûterait des secondes — et pas d'état : l'appelant
//! (`DlnaOutput`) tient ses propres URLs et décide quand rejouer.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use super::ssdp::{
    device_id_from_usn, host_from_location, parse_ssdp_response, port_from_location,
};
use super::xml_parser::fetch_device_description;

/// Port SSDP normalisé. Injectable par [`rechercher_location_par_unicast`]
/// pour qu'un banc puisse tenir un faux répondeur sans privilège sur 1900.
pub const PORT_SSDP: u16 = 1900;

/// Budget d'attente de la réponse unicast. L'appareil du relevé répond sous la
/// seconde ; au-delà de deux, c'est qu'il ne répondra pas, et l'appelant doit
/// rendre l'erreur d'origine sans doubler le délai d'un appareil éteint.
pub const BUDGET_REPONSE: Duration = Duration::from_secs(2);

/// Les URLs de contrôle d'un renderer, résolues en ABSOLU depuis son
/// descriptif — même règle que `resolve_control_url` côté serveur : un chemin
/// relatif se greffe sur `host:port` de la `LOCATION`, une URL absolue est
/// gardée telle quelle (radios Frontier Silicon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlsDeControle {
    pub location: String,
    pub av_transport: String,
    pub rendering_control: String,
    pub connection_manager: Option<String>,
    /// `eventSubURL` absolues, par clé de service (`avtransport`,
    /// `renderingcontrol`), vides omises.
    pub event_sub_urls: std::collections::HashMap<String, String>,
}

/// `M-SEARCH` UNICAST vers `ip:port_ssdp`, pour le seul appareil `device_id`
/// (`uuid:…`, la forme que porte `DiscoveredDevice::id` depuis l'USN).
///
/// Trois datagrammes partent d'un coup, et la première réponse dont l'USN
/// porte notre UDN gagne :
/// - `ST: uuid:<udn>` avec `HOST: 239.255.255.250:1900` — la forme que la
///   plupart des outils envoient, unicast compris, et que Platinum accepte ;
/// - `ST: uuid:<udn>` avec `HOST: <ip>:<port>` — la forme que la spec UPnP 1.1
///   (§1.3.2) prescrit pour l'unicast, pour les piles qui la vérifient ;
/// - `ST: ssdp:all` — filet pour une pile qui ne répond pas aux recherches par
///   UDN ; la réponse est filtrée sur l'USN, donc rien d'autre ne peut passer.
///
/// Rend la `LOCATION` annoncée, ou `None` si rien n'est venu dans `budget`.
pub async fn rechercher_location_par_unicast(
    ip: &str,
    port_ssdp: u16,
    device_id: &str,
    budget: Duration,
) -> Option<String> {
    let cible: SocketAddr = match format!("{ip}:{port_ssdp}").parse() {
        Ok(c) => c,
        Err(e) => {
            debug!(ip, port_ssdp, error = %e, "redecouverte_adresse_invalide");
            return None;
        }
    };
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "redecouverte_socket_udp_refuse");
            return None;
        }
    };
    let st_udn = if device_id.starts_with("uuid:") {
        device_id.to_string()
    } else {
        format!("uuid:{device_id}")
    };
    let hote_multicast = format!("239.255.255.250:{PORT_SSDP}");
    let hote_unicast = format!("{ip}:{port_ssdp}");
    for (hote, st) in [
        (hote_multicast.as_str(), st_udn.as_str()),
        (hote_unicast.as_str(), st_udn.as_str()),
        (hote_multicast.as_str(), "ssdp:all"),
    ] {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: {hote}\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 1\r\n\
             ST: {st}\r\n\
             \r\n"
        );
        if let Err(e) = socket.send_to(msg.as_bytes(), cible).await {
            debug!(%cible, st, error = %e, "redecouverte_msearch_envoi_echoue");
        }
    }
    let mut buf = [0u8; 4096];
    let echeance = tokio::time::Instant::now() + budget;
    loop {
        let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            break;
        }
        match tokio::time::timeout(reste, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, de))) => {
                let Some(resp) = parse_ssdp_response(&buf[..len]) else {
                    continue;
                };
                if device_id_from_usn(&resp.usn).eq_ignore_ascii_case(device_id) {
                    debug!(de = %de, location = %resp.location, "redecouverte_msearch_reponse");
                    return Some(resp.location);
                }
                debug!(de = %de, usn = %resp.usn, "redecouverte_msearch_autre_usn_ignore");
            }
            Ok(Err(e)) => {
                debug!(error = %e, "redecouverte_msearch_recv_erreur");
            }
            Err(_) => break,
        }
    }
    None
}

/// Relit le descriptif à `location` et en tire les URLs de contrôle absolues.
///
/// Refuse un descriptif sans `AVTransport` ni `RenderingControl` : ce n'est
/// pas un renderer, et remplacer des URLs valides par du vide serait pire que
/// le refus de connexion qu'on cherche à soigner.
pub async fn relire_urls_de_controle(location: &str) -> Result<UrlsDeControle, String> {
    let desc = fetch_device_description(location).await?;
    let hote = host_from_location(location).unwrap_or_default();
    let port = port_from_location(location);
    let resoudre = |chemin: &str| resoudre_url(&hote, port, chemin);
    let services = desc.service_urls();
    let av_transport = services
        .get("avtransport")
        .filter(|p| !p.trim().is_empty())
        .map(|p| resoudre(p))
        .ok_or_else(|| format!("descriptif {location} sans AVTransport"))?;
    let rendering_control = services
        .get("renderingcontrol")
        .filter(|p| !p.trim().is_empty())
        .map(|p| resoudre(p))
        .ok_or_else(|| format!("descriptif {location} sans RenderingControl"))?;
    let connection_manager = services
        .get("connectionmanager")
        .filter(|p| !p.trim().is_empty())
        .map(|p| resoudre(p));
    let event_sub_urls = desc
        .event_sub_urls()
        .into_iter()
        .filter(|(k, p)| (k == "avtransport" || k == "renderingcontrol") && !p.trim().is_empty())
        .map(|(k, p)| (k, resoudre(&p)))
        .collect();
    Ok(UrlsDeControle {
        location: location.to_string(),
        av_transport,
        rendering_control,
        connection_manager,
        event_sub_urls,
    })
}

/// Le geste complet : `M-SEARCH` unicast, puis relecture du descriptif.
///
/// `Err` porte le motif que l'appelant accole à l'erreur d'origine — il ne la
/// remplace jamais : le `10061` reste la première information du journal.
pub async fn redecouvrir(
    ip: &str,
    port_ssdp: u16,
    device_id: &str,
    budget: Duration,
) -> Result<UrlsDeControle, String> {
    let Some(location) = rechercher_location_par_unicast(ip, port_ssdp, device_id, budget).await
    else {
        return Err(format!(
            "aucune réponse M-SEARCH de {ip} pour {device_id} en {} ms",
            budget.as_millis()
        ));
    };
    let urls = relire_urls_de_controle(&location).await?;
    info!(
        id = %device_id,
        location = %urls.location,
        av_transport = %urls.av_transport,
        "dlna_redecouverte_urls_relues"
    );
    Ok(urls)
}

/// Un `controlURL` du descriptif, rendu absolu. Copie conforme de
/// `discovery_setup::resolve_control_url` (tune-server), qui n'est pas visible
/// d'ici.
pub(crate) fn resoudre_url(hote: &str, port: u16, chemin: &str) -> String {
    if chemin.starts_with("http://") || chemin.starts_with("https://") {
        chemin.to_string()
    } else {
        let sep = if chemin.starts_with('/') { "" } else { "/" };
        format!("http://{hote}:{port}{sep}{chemin}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_chemin_relatif_se_greffe_sur_la_location_un_absolu_reste() {
        assert_eq!(
            resoudre_url("192.168.1.17", 1838, "/AVTransport/x/control.xml"),
            "http://192.168.1.17:1838/AVTransport/x/control.xml"
        );
        assert_eq!(
            resoudre_url("192.168.1.17", 1838, "AVTransport/control.xml"),
            "http://192.168.1.17:1838/AVTransport/control.xml"
        );
        assert_eq!(
            resoudre_url("192.168.1.17", 1838, "http://192.168.1.17:8080/ctl"),
            "http://192.168.1.17:8080/ctl"
        );
    }

    /// Le `M-SEARCH` est UNICAST et vise le SEUL appareil demandé : une réponse
    /// d'un autre UDN est ignorée, celle du bon est rendue avec sa `LOCATION`.
    #[tokio::test]
    async fn la_recherche_unicast_ne_retient_que_l_udn_demande() {
        let repondeur = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = repondeur.local_addr().unwrap().port();
        let udn = "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE";
        let serveur = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, de) = repondeur.recv_from(&mut buf).await.unwrap();
            let texte = String::from_utf8_lossy(&buf[..n]).to_string();
            // D'abord un intrus, puis le bon.
            let intrus = "HTTP/1.1 200 OK\r\nLOCATION: http://127.0.0.1:9/autre.xml\r\n\
                          USN: uuid:autre::upnp:rootdevice\r\nST: upnp:rootdevice\r\n\r\n";
            repondeur.send_to(intrus.as_bytes(), de).await.unwrap();
            let bon = format!(
                "HTTP/1.1 200 OK\r\nLOCATION: http://127.0.0.1:1838/description.xml\r\n\
                 USN: {udn}\r\nST: {udn}\r\n\r\n"
            );
            repondeur.send_to(bon.as_bytes(), de).await.unwrap();
            texte
        });
        let loc = rechercher_location_par_unicast("127.0.0.1", port, udn, BUDGET_REPONSE).await;
        assert_eq!(
            loc.as_deref(),
            Some("http://127.0.0.1:1838/description.xml")
        );
        let requete = serveur.await.unwrap();
        assert!(requete.starts_with("M-SEARCH * HTTP/1.1\r\n"), "{requete}");
        assert!(requete.contains(&format!("ST: {udn}\r\n")), "{requete}");
        assert!(requete.contains("MAN: \"ssdp:discover\"\r\n"), "{requete}");
    }

    /// Personne ne répond : on rend `None` dans le budget, pas au-delà.
    #[tokio::test]
    async fn sans_reponse_la_recherche_rend_none_dans_le_budget() {
        let port = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap().port()
        };
        let debut = std::time::Instant::now();
        let loc = rechercher_location_par_unicast(
            "127.0.0.1",
            port,
            "uuid:personne",
            Duration::from_millis(300),
        )
        .await;
        assert!(loc.is_none());
        assert!(
            debut.elapsed() < Duration::from_secs(2),
            "le budget n'est pas tenu : {:?}",
            debut.elapsed()
        );
    }
}
