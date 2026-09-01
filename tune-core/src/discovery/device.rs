use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Local,
    Dlna,
    Airplay,
    Chromecast,
    Bluos,
    Openhome,
    Squeezebox,
    Oaat,
}

impl OutputType {
    pub fn priority(self) -> u8 {
        match self {
            Self::Oaat => 8,
            Self::Openhome => 7,
            Self::Bluos => 6,
            Self::Squeezebox => 5,
            Self::Dlna => 4,
            Self::Chromecast => 3,
            Self::Airplay => 2,
            Self::Local => 1,
        }
    }
}

impl std::fmt::Display for OutputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Dlna => write!(f, "dlna"),
            Self::Airplay => write!(f, "airplay"),
            Self::Chromecast => write!(f, "chromecast"),
            Self::Bluos => write!(f, "bluos"),
            Self::Openhome => write!(f, "openhome"),
            Self::Squeezebox => write!(f, "squeezebox"),
            Self::Oaat => write!(f, "oaat"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub id: String,
    pub name: String,
    pub device_type: OutputType,
    pub host: String,
    pub port: u16,
    pub available: bool,
    pub capabilities: HashMap<String, serde_json::Value>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub location: Option<String>,
    pub airplay_version: Option<String>,
    pub mac_address: Option<String>,
    /// L'identifiant que l'appareil annonce lui-meme, brut.
    ///
    /// A ne JAMAIS deriver ni remplacer : c'est la seule valeur dont la
    /// stabilite soit garantie par le protocole, et c'est ce qui permet a une
    /// zone de survivre a un changement d'adresse IP (#1528).
    ///
    /// Distinct de `mac_address`, qui est une identite « au mieux » :
    /// `mac::enrich_identity` la reecrit depuis l'ARP quand la valeur annoncee
    /// n'est pas une MAC — le TXT `id` d'un Chromecast est un UUID — et bascule
    /// donc entre deux valeurs selon l'etat du cache ARP. La confondre avec une
    /// identite stable remplacerait le defaut par un autre, plus difficile a
    /// reproduire.
    pub stable_id: Option<String>,
}

impl DiscoveredDevice {
    pub fn new(id: String, name: String, device_type: OutputType, host: String, port: u16) -> Self {
        Self {
            id,
            name,
            device_type,
            host,
            port,
            available: true,
            capabilities: HashMap::new(),
            manufacturer: None,
            model: None,
            location: None,
            airplay_version: None,
            mac_address: None,
            stable_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternative {
    pub id: String,
    pub name: String,
    pub device_type: OutputType,
}

/// Ce qui fait qu'on tient deux annonces pour UN seul appareil.
///
/// L'hote seul ne suffit pas : un serveur Lyrion annonce chacune de ses
/// platines Squeezebox en renderer DLNA portant SON adresse a lui, et un NAS
/// multi-renderer fait pareil. Regrouper sur l'adresse seule les ramenait
/// toutes a une (#2942). Le nom, lui, distingue les platines entre elles tout
/// en restant identique d'un protocole a l'autre pour un meme appareil, ce qui
/// preserve le repli d'origine (#1880, #2452).
#[derive(Clone, PartialEq, Eq, Hash)]
enum GroupKey {
    /// Cas courant : meme adresse ET meme nom annonce.
    Named { host: String, name: String },
    /// Aucun nom exploitable : on retombe sur (hote, port), faute de mieux.
    Anonymous { host: String, port: u16 },
}

fn group_key(dev: &DiscoveredDevice) -> GroupKey {
    let name = dev.name.trim().to_lowercase();
    if name.is_empty() {
        GroupKey::Anonymous {
            host: dev.host.clone(),
            port: dev.port,
        }
    } else {
        GroupKey::Named {
            host: dev.host.clone(),
            name,
        }
    }
}

/// Replie les annonces multiples d'un meme appareil, et SEULEMENT elles.
///
/// L'identite retenue est celle que l'appareil annonce lui-meme quand il en
/// annonce une (`stable_id`, cf. #1528) : deux annonces qui portent le meme
/// `stable_id` sont le meme appareil, meme s'il s'est renomme entre les deux.
/// A defaut — un `stable_id` differe d'un protocole a l'autre pour un meme
/// appareil, le TXT `deviceid` mDNS et l'UDN SSDP n'ont rien a voir — on
/// retombe sur (hote, nom).
///
/// L'ordre de sortie suit l'ordre de decouverte : le `HashMap` d'origine le
/// rendait dependant du hasard du hachage.
pub fn dedup_devices(devices: Vec<DiscoveredDevice>) -> Vec<DiscoveredDevice> {
    // stable_id -> cle du groupe ouvert par la premiere annonce qui le portait.
    let mut by_stable: HashMap<String, GroupKey> = HashMap::new();
    let mut index: HashMap<GroupKey, usize> = HashMap::new();
    let mut groups: Vec<Vec<DiscoveredDevice>> = Vec::new();

    for dev in devices {
        if dev
            .manufacturer
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains("mozaik"))
        {
            continue;
        }
        let natural = group_key(&dev);
        let key = match dev
            .stable_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(stable) => by_stable
                .entry(stable.to_string())
                .or_insert(natural)
                .clone(),
            None => natural,
        };
        let slot = *index.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(dev);
    }

    let mut result = Vec::with_capacity(groups.len());
    for mut group in groups {
        group.sort_by_key(|b| std::cmp::Reverse(b.device_type.priority()));
        let mut primary = group.remove(0);
        if !group.is_empty() {
            let alts: Vec<Alternative> = group
                .iter()
                .map(|d| Alternative {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    device_type: d.device_type,
                })
                .collect();
            primary.capabilities.insert(
                "alternatives".to_string(),
                serde_json::to_value(alts).unwrap_or_default(),
            );
        }
        result.push(primary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_highest_priority() {
        let devices = vec![
            DiscoveredDevice::new(
                "dlna-1".into(),
                "Speaker".into(),
                OutputType::Dlna,
                "192.168.1.50".into(),
                1400,
            ),
            DiscoveredDevice::new(
                "oh-1".into(),
                "Speaker".into(),
                OutputType::Openhome,
                "192.168.1.50".into(),
                1400,
            ),
        ];
        let result = dedup_devices(devices);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].device_type, OutputType::Openhome);
        assert!(result[0].capabilities.contains_key("alternatives"));
    }

    /// Dix platines Squeezebox annoncees en DLNA par le meme serveur Lyrion :
    /// meme adresse, un nom et un UDN par platine (#2942, fil forum 208).
    fn platines_derriere_un_lms() -> Vec<DiscoveredDevice> {
        const PIECES: [&str; 10] = [
            "Salon", "Cuisine", "Chambre", "Bureau", "Cave", "Garage", "Terrasse", "Atelier",
            "Couloir", "Grenier",
        ];
        PIECES
            .iter()
            .enumerate()
            .map(|(n, piece)| {
                let mut d = DiscoveredDevice::new(
                    format!("dlna-uuid:lms-{n}"),
                    format!("Squeezebox {piece}"),
                    OutputType::Dlna,
                    "192.168.1.10".into(),
                    9000 + n as u16,
                );
                d.stable_id = Some(format!("uuid:lms-{n}"));
                d
            })
            .collect()
    }

    /// Le meme Marantz sous ses deux identites : mDNS AirPlay et SSDP OpenHome,
    /// meme adresse, meme nom, ports et `stable_id` differents (#1880, #2452).
    fn marantz_deux_identites() -> Vec<DiscoveredDevice> {
        let mut airplay = DiscoveredDevice::new(
            "airplay-00:06:78:7C:2E:26".into(),
            "Marantz ND8006".into(),
            OutputType::Airplay,
            "192.168.1.50".into(),
            7000,
        );
        airplay.stable_id = Some("00:06:78:7C:2E:26".into());
        let mut openhome = DiscoveredDevice::new(
            "openhome-uuid:56fcb4ae".into(),
            "Marantz ND8006".into(),
            OutputType::Openhome,
            "192.168.1.50".into(),
            1400,
        );
        openhome.stable_id = Some("uuid:56fcb4ae".into());
        vec![airplay, openhome]
    }

    #[test]
    fn dedup_garde_les_appareils_distincts_derriere_une_meme_adresse() {
        // #2942 — le fait mesure est le NOMBRE d'appareils rendus. Avant le
        // correctif, la cle de regroupement etait l'adresse et rien d'autre :
        // les dix platines n'en donnaient qu'UNE, les neuf autres rabattues
        // dans capabilities["alternatives"], qui ne porte ni hote ni port.
        let mut devices = platines_derriere_un_lms();
        // TEMOIN, vert des deux cotes : le Marantz a deux identites doit
        // toujours ne produire qu'UNE entree. C'est la raison d'etre du repli
        // (#1880, #2452) ; le correctif ne doit pas l'emporter avec lui.
        devices.extend(marantz_deux_identites());

        let result = dedup_devices(devices);

        assert_eq!(
            result.len(),
            11,
            "dix platines distinctes + un Marantz replie = 11 entrees, obtenu : {:?}",
            result.iter().map(|d| &d.name).collect::<Vec<_>>()
        );

        let platines: Vec<&str> = result
            .iter()
            .filter(|d| d.host == "192.168.1.10")
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            platines.len(),
            10,
            "les dix platines doivent survivre, obtenu : {platines:?}"
        );
        // Chacune garde son propre port : c'est ce qui la rend adressable.
        let ports: std::collections::HashSet<u16> = result
            .iter()
            .filter(|d| d.host == "192.168.1.10")
            .map(|d| d.port)
            .collect();
        assert_eq!(ports.len(), 10, "chaque platine garde son port, {ports:?}");
        // Et aucune n'est rabattue en alternative d'une autre.
        assert!(
            result
                .iter()
                .filter(|d| d.host == "192.168.1.10")
                .all(|d| !d.capabilities.contains_key("alternatives")),
            "aucune platine ne doit etre repliee dans une autre"
        );

        // TEMOIN : le Marantz, lui, reste une seule entree, l'identite UPnP en
        // primaire (priorite Openhome > Airplay), l'AirPlay en alternative.
        let marantz: Vec<&DiscoveredDevice> =
            result.iter().filter(|d| d.host == "192.168.1.50").collect();
        assert_eq!(
            marantz.len(),
            1,
            "les deux identites d'un meme appareil restent UNE entree, obtenu : {marantz:?}"
        );
        assert_eq!(marantz[0].device_type, OutputType::Openhome);
        assert!(marantz[0].capabilities.contains_key("alternatives"));
    }

    #[test]
    fn dedup_replie_une_re_annonce_qui_a_change_de_nom() {
        // Un renderer renomme entre deux annonces garde son UDN : c'est la
        // seule identite garantie par le protocole (#1528), et elle prime sur
        // le nom. Sans ce rattachement, (hote, nom) en ferait deux appareils.
        let mut avant = DiscoveredDevice::new(
            "dlna-uuid:aa".into(),
            "Salon".into(),
            OutputType::Dlna,
            "192.168.1.10".into(),
            9000,
        );
        avant.stable_id = Some("uuid:aa".into());
        let mut apres = avant.clone();
        apres.name = "Sejour".into();

        let result = dedup_devices(vec![avant, apres]);
        assert_eq!(
            result.len(),
            1,
            "meme stable_id = meme appareil, obtenu : {:?}",
            result.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn dedup_conserve_l_ordre_de_decouverte() {
        // Le HashMap d'origine rendait l'ordre de sortie dependant du hachage.
        let devices = platines_derriere_un_lms();
        let attendus: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
        let rendus: Vec<String> = dedup_devices(devices).into_iter().map(|d| d.name).collect();
        assert_eq!(rendus, attendus);
    }

    #[test]
    fn dedup_filters_self() {
        let mut dev = DiscoveredDevice::new(
            "self".into(),
            "Tune".into(),
            OutputType::Dlna,
            "127.0.0.1".into(),
            8888,
        );
        dev.manufacturer = Some("Mozaik Labs".into());
        let result = dedup_devices(vec![dev]);
        assert!(result.is_empty());
    }
}
