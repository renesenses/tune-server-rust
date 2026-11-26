//! Trouver les serveurs Tune du réseau : `_tune-server._tcp.local.`, le service
//! que le serveur annonce (`tune-core/src/discovery/mdns.rs`, `TUNE_SERVICE`),
//! avec les TXT `version` et `path=/api/v1`.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;

use crate::adresse::{base, ip_locale};

pub const SERVICE_TUNE: &str = "_tune-server._tcp.local.";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Trouve {
    pub nom: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// L'adresse à appeler parmi celles annoncées : IPv4 locale d'abord, puis
/// IPv6 locale ; jamais une adresse publique. Ordre stable.
pub fn meilleure_adresse(ips: &[IpAddr]) -> Option<IpAddr> {
    let mut locales: Vec<IpAddr> = ips
        .iter()
        .copied()
        .filter(|ip| ip_locale(*ip) && !ip.is_loopback())
        .collect();
    locales.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    locales.first().copied()
}

/// Le nom lisible d'une instance : `Tune (salon)._tune-server._tcp.local.` → `Tune (salon)`.
pub fn nom_lisible(complet: &str) -> String {
    complet
        .strip_suffix(SERVICE_TUNE)
        .unwrap_or(complet)
        .trim_end_matches('.')
        .to_string()
}

/// Écoute pendant `duree` et rend les serveurs trouvés. Une erreur (pare-feu,
/// pas d'interface) rend une liste vide : la saisie manuelle prend le relais.
pub fn chercher(duree: Duration) -> Vec<Trouve> {
    let Ok(demon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(recepteur) = demon.browse(SERVICE_TUNE) else {
        let _ = demon.shutdown();
        return Vec::new();
    };
    let fin = Instant::now() + duree;
    let mut trouves: BTreeMap<String, Trouve> = BTreeMap::new();
    while let Some(reste) = fin.checked_duration_since(Instant::now()) {
        match recepteur.recv_timeout(reste) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let ips: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .collect();
                if let Some(ip) = meilleure_adresse(&ips) {
                    let b = base(ip, info.get_port());
                    trouves.insert(
                        b.clone(),
                        Trouve {
                            nom: nom_lisible(info.get_fullname()),
                            base: b,
                            version: info.get_property_val_str("version").map(str::to_string),
                        },
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = demon.stop_browse(SERVICE_TUNE);
    let _ = demon.shutdown();
    trouves.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adresse_ipv4_locale_preferee_et_publique_ecartee() {
        let ips: Vec<IpAddr> = ["8.8.8.8", "fe80::1", "192.168.1.18"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(
            meilleure_adresse(&ips),
            Some("192.168.1.18".parse().unwrap())
        );
        let publiques: Vec<IpAddr> = vec!["8.8.8.8".parse().unwrap()];
        assert_eq!(meilleure_adresse(&publiques), None);
    }

    #[test]
    fn nom_sans_le_type_de_service() {
        assert_eq!(
            nom_lisible("Tune (salon)._tune-server._tcp.local."),
            "Tune (salon)"
        );
    }
}
