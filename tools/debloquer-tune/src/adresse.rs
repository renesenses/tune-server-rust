//! L'adresse du Tune, saisie ou découverte, et la garde « réseau local seulement ».
//!
//! Le mot de passe ne doit JAMAIS sortir du réseau local. Toute adresse dont la
//! résolution donne une IP publique est refusée avant le moindre appel.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Port du serveur Tune. Sous Tune OS, le port 80 n'est qu'un relais vers lui.
pub const PORT_TUNE: u16 = 8888;

/// Une adresse est-elle sur le réseau local (ou cette machine) ?
pub fn ip_locale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_locale(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_locale(v4);
            }
            v6_locale(v6)
        }
    }
}

fn v4_locale(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        // 100.64.0.0/10 : réseau partagé (Tailscale et consorts) — non routé sur Internet.
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
}

fn v6_locale(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    ip.is_loopback()
        // fe80::/10 lien local, fc00::/7 adresses uniques locales.
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xfe00) == 0xfc00
}

/// Hôte et port extraits de la saisie (`192.168.1.18`, `tune.local:8888`,
/// `http://192.168.1.18/`…). Le port manquant reste `None` : l'appelant essaie
/// 8888 puis 80.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saisie {
    pub hote: String,
    pub port: Option<u16>,
}

pub fn lire_saisie(texte: &str) -> Result<Saisie, &'static str> {
    let mut t = texte.trim();
    for prefixe in ["http://", "HTTP://", "https://", "HTTPS://"] {
        if let Some(r) = t.strip_prefix(prefixe) {
            t = r;
        }
    }
    // Rien après l'hôte : ni chemin, ni identifiants.
    let t = t.split('/').next().unwrap_or("");
    if t.is_empty() {
        return Err("adresse_vide");
    }
    if t.contains('@') || t.contains(char::is_whitespace) {
        return Err("adresse_invalide");
    }
    // IPv6 entre crochets, avec ou sans port.
    if let Some(reste) = t.strip_prefix('[') {
        let (hote, apres) = reste.split_once(']').ok_or("adresse_invalide")?;
        let port = match apres.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| "adresse_invalide")?),
            None if apres.is_empty() => None,
            None => return Err("adresse_invalide"),
        };
        hote.parse::<Ipv6Addr>().map_err(|_| "adresse_invalide")?;
        return Ok(Saisie {
            hote: hote.to_string(),
            port,
        });
    }
    if t.parse::<Ipv6Addr>().is_ok() {
        return Ok(Saisie {
            hote: t.to_string(),
            port: None,
        });
    }
    let (hote, port) = match t.rsplit_once(':') {
        Some((h, p)) => (h, Some(p.parse::<u16>().map_err(|_| "adresse_invalide")?)),
        None => (t, None),
    };
    if hote.is_empty()
        || !hote
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err("adresse_invalide");
    }
    if port == Some(0) {
        return Err("adresse_invalide");
    }
    Ok(Saisie {
        hote: hote.to_string(),
        port,
    })
}

/// Résout l'hôte et vérifie que TOUTES ses adresses sont locales. Rend la
/// première adresse locale résolue : c'est elle qu'on appelle, ce qui ferme la
/// porte à un nom qui changerait de réponse entre la vérification et l'appel.
pub fn resoudre_localement(hote: &str, port: u16) -> Result<IpAddr, &'static str> {
    if let Ok(ip) = hote.parse::<IpAddr>() {
        return if ip_locale(ip) {
            Ok(ip)
        } else {
            Err("adresse_hors_reseau_local")
        };
    }
    let ips: Vec<IpAddr> = (hote, port)
        .to_socket_addrs()
        .map_err(|_| "adresse_introuvable")?
        .map(|a| a.ip())
        .collect();
    if ips.is_empty() {
        return Err("adresse_introuvable");
    }
    if !ips.iter().all(|ip| ip_locale(*ip)) {
        return Err("adresse_hors_reseau_local");
    }
    Ok(ips[0])
}

/// `http://ip:port` prêt à recevoir `/api/v1/…`.
pub fn base(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

/// Relit une base déjà produite par [`base`] (renvoyée par la page) et
/// revérifie qu'elle est locale : la page n'est pas crue sur parole.
pub fn relire_base(texte: &str) -> Result<String, &'static str> {
    let reste = texte.strip_prefix("http://").ok_or("adresse_invalide")?;
    let saisie = lire_saisie(reste)?;
    let port = saisie.port.ok_or("adresse_invalide")?;
    let ip: IpAddr = saisie.hote.parse().map_err(|_| "adresse_invalide")?;
    if !ip_locale(ip) {
        return Err("adresse_hors_reseau_local");
    }
    Ok(base(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reseau_local_accepte_internet_refuse() {
        for ip in [
            "192.168.1.18",
            "10.0.0.2",
            "172.16.5.4",
            "169.254.1.1",
            "127.0.0.1",
            "100.100.1.1",
            "fe80::1",
            "fd00::5",
            "::1",
            "::ffff:192.168.1.18",
        ] {
            assert!(ip_locale(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "2001:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(!ip_locale(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn saisies_usuelles() {
        assert_eq!(
            lire_saisie("192.168.1.18").unwrap(),
            Saisie {
                hote: "192.168.1.18".into(),
                port: None
            }
        );
        assert_eq!(
            lire_saisie(" http://192.168.1.18:8888/ ").unwrap(),
            Saisie {
                hote: "192.168.1.18".into(),
                port: Some(8888)
            }
        );
        assert_eq!(lire_saisie("tune-os.local").unwrap().hote, "tune-os.local");
        assert_eq!(
            lire_saisie("[fe80::1]:8888").unwrap(),
            Saisie {
                hote: "fe80::1".into(),
                port: Some(8888)
            }
        );
        for mauvaise in [
            "",
            "a b",
            "user@192.168.1.18",
            "192.168.1.18:x",
            "192.168.1.18:0",
            "exemple.fr\\x",
        ] {
            assert!(lire_saisie(mauvaise).is_err(), "{mauvaise}");
        }
    }

    #[test]
    fn une_ip_publique_est_refusee_avant_tout_appel() {
        assert_eq!(
            resoudre_localement("8.8.8.8", 8888),
            Err("adresse_hors_reseau_local")
        );
        assert!(resoudre_localement("192.168.1.18", 8888).is_ok());
    }

    #[test]
    fn la_base_renvoyee_par_la_page_est_reverifiee() {
        assert_eq!(
            relire_base("http://192.168.1.18:8888").unwrap(),
            "http://192.168.1.18:8888"
        );
        assert_eq!(
            relire_base("http://8.8.8.8:8888"),
            Err("adresse_hors_reseau_local")
        );
        assert!(relire_base("http://tune.local:8888").is_err());
        assert!(relire_base("https://192.168.1.18:8888").is_err());
    }
}
