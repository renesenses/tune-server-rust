//! Découverte des Tune Server du réseau local (#9255, fil du forum 2968).
//!
//! Le serveur s'annonce en mDNS sur `_tune-server._tcp` (`tune-core`,
//! `discovery::mdns::register_self`) avec son nom, son port et un TXT
//! `version`. Les apps iOS/tvOS le trouvent ainsi ; l'app de bureau, elle, ne
//! connaissait que `http://localhost:8888` ou une adresse tapée à la main —
//! un serveur posé sur une AUTRE machine du réseau n'était jamais trouvé.
//!
//! Le parcours et le choix d'adresse sont séparés : `decouvrir` parle au
//! réseau, tout le reste est pur et éprouvé sans réseau.
//!
//! macOS demande l'autorisation « Réseau local » au premier parcours ; elle
//! n'est accordée que si `Info.plist` porte `NSLocalNetworkUsageDescription`
//! et déclare ce type dans `NSBonjourServices` (garde : test plus bas).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Le type de service, tel que le serveur l'annonce.
pub const SERVICE: &str = "_tune-server._tcp.local.";

/// Le même type, tel que `NSBonjourServices` l'écrit (sans le domaine). Lu
/// par le banc qui garde `Info.plist`.
#[cfg_attr(not(test), allow(dead_code))]
pub const TYPE_BONJOUR: &str = "_tune-server._tcp";

/// Durée d'un parcours. Un serveur répond en quelques dizaines de
/// millisecondes ; trois secondes laissent passer une seconde question mDNS
/// (le démon la répète) sans faire attendre l'utilisateur.
pub const DUREE_PARCOURS: Duration = Duration::from_secs(3);

/// Un serveur trouvé sur le réseau.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServeurTrouve {
    /// Le nom annoncé (« Tune (salon) »…), à montrer à l'utilisateur.
    pub nom: String,
    /// L'adresse à laquelle le joindre : `http://<ip>:<port>`.
    pub url: String,
    /// Le nom d'hôte mDNS (`salon.local.`), pour départager deux homonymes.
    pub hote: String,
    /// La version que le serveur annonce dans son TXT, si elle y est.
    pub version: Option<String>,
    /// Le nom complet du service : l'identité stable d'un serveur, qui ne
    /// change pas quand son adresse IP change.
    pub cle: String,
}

/// Le nom d'instance d'un nom complet de service :
/// `Tune (salon)._tune-server._tcp.local.` → `Tune (salon)`.
pub fn nom_d_instance(nom_complet: &str) -> String {
    let nom = nom_complet
        .strip_suffix(SERVICE)
        .map(|n| n.trim_end_matches('.'))
        .unwrap_or(nom_complet);
    // Le démon garde les échappements DNS des caractères spéciaux.
    nom.replace("\\032", " ")
        .replace("\\.", ".")
        .replace("\\\\", "\\")
}

/// Rang d'une adresse : plus petit = meilleure.
///
/// - 0 : 192.168/16, le LAN domestique ;
/// - 1 : 10/8 ;
/// - 2 : 172.16/12 — après les deux autres : c'est aussi la plage des ponts
///   Docker (172.17.0.1), injoignable depuis une autre machine ;
/// - 3 : autre IPv4 routable ;
/// - 4 : IPv4 lien-local (169.254/16, réseau sans DHCP) ;
/// - 5 : IPv6 globale ou unique-locale ;
/// - 6 : boucle locale (le serveur tourne sur CETTE machine et ne connaissait
///   pas encore son adresse au moment de s'annoncer) ;
/// - `None` : inutilisable par une page web — IPv6 lien-local (`fe80::`), qui
///   exige un identifiant d'interface qu'une URL http ne porte pas,
///   adresse non spécifiée, multidiffusion.
fn rang(ip: &IpAddr) -> Option<u8> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() {
                None
            } else if v4.is_loopback() {
                Some(6)
            } else if v4.is_private() {
                Some(match v4.octets()[0] {
                    192 => 0,
                    10 => 1,
                    _ => 2,
                })
            } else if v4.is_link_local() {
                Some(4)
            } else {
                Some(3)
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                None
            } else if v6.is_loopback() {
                Some(6)
            } else if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                None
            } else {
                Some(5)
            }
        }
    }
}

/// La meilleure adresse pour joindre un serveur qui en annonce plusieurs.
///
/// Un serveur à deux interfaces (Wi-Fi + Ethernet, ou Docker) en annonce
/// plusieurs ; on préfère celle d'un LAN privé. À rang égal, la plus petite,
/// pour que le choix ne dépende pas de l'ordre d'arrivée des réponses.
pub fn meilleure_adresse(adresses: &[IpAddr]) -> Option<IpAddr> {
    adresses
        .iter()
        .filter_map(|ip| rang(ip).map(|r| (r, *ip)))
        .min()
        .map(|(_, ip)| ip)
}

/// L'adresse http d'un serveur (IPv6 entre crochets).
pub fn url_de(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

/// Un serveur à partir de ce que mDNS en a résolu ; `None` s'il n'a aucune
/// adresse utilisable ou pas de port.
pub fn serveur_depuis(
    nom_complet: &str,
    hote: &str,
    adresses: &[IpAddr],
    port: u16,
    version: Option<&str>,
) -> Option<ServeurTrouve> {
    if port == 0 {
        return None;
    }
    let ip = meilleure_adresse(adresses)?;
    Some(ServeurTrouve {
        nom: nom_d_instance(nom_complet),
        url: url_de(ip, port),
        hote: hote.trim_end_matches('.').to_string(),
        version: version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        cle: nom_complet.to_string(),
    })
}

/// Une liste sans doublon (un serveur résolu deux fois garde sa dernière
/// résolution), triée par nom puis par adresse : l'ordre ne dépend pas de
/// celui des réponses.
pub fn fusionner(trouves: impl IntoIterator<Item = ServeurTrouve>) -> Vec<ServeurTrouve> {
    let mut par_cle: BTreeMap<String, ServeurTrouve> = BTreeMap::new();
    for s in trouves {
        par_cle.insert(s.cle.clone(), s);
    }
    let mut liste: Vec<_> = par_cle.into_values().collect();
    liste.sort_by(|a, b| {
        a.nom
            .to_lowercase()
            .cmp(&b.nom.to_lowercase())
            .then_with(|| a.url.cmp(&b.url))
    });
    liste
}

/// Ce que l'app fait au lancement, une fois le réseau parcouru.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoixAuDemarrage {
    /// Le serveur retenu répond : rien à changer.
    Garder,
    /// Se connecter à ce serveur sans rien demander.
    Adopter(ServeurTrouve),
    /// Montrer la liste (zéro, ou plusieurs serveurs) avec la saisie manuelle.
    Proposer,
}

/// Que faire au lancement ?
///
/// - `cle_retenue` : l'identité mDNS du serveur choisi la dernière fois, s'il
///   a été choisi dans la liste (`None` : jamais choisi, ou adresse tapée) ;
/// - `choisi` : l'utilisateur a-t-il déjà retenu une adresse (liste ou
///   saisie) ? `false` = l'adresse par défaut, `localhost:8888` ;
/// - `joignable` : l'adresse retenue répond-elle ?
///
/// Règles :
/// 1. l'adresse retenue répond → on la garde (un Tune Server sur ce Mac
///    l'emporte donc sur ceux du réseau) ;
/// 2. le serveur choisi la dernière fois est revu sous une autre adresse
///    (bail DHCP renouvelé) → on le suit ;
/// 3. rien n'a jamais été choisi et UN seul serveur répond sur le réseau →
///    on s'y connecte : c'est le cas de Rémi, serveur sur une autre machine ;
/// 4. sinon (aucun, plusieurs, ou le serveur choisi a disparu) → la liste.
///    On ne bascule jamais en silence d'un serveur choisi vers un autre.
pub fn choisir_au_demarrage(
    cle_retenue: Option<&str>,
    choisi: bool,
    joignable: bool,
    trouves: &[ServeurTrouve],
) -> ChoixAuDemarrage {
    if joignable {
        return ChoixAuDemarrage::Garder;
    }
    if let Some(cle) = cle_retenue {
        if let Some(s) = trouves.iter().find(|s| s.cle == cle) {
            return ChoixAuDemarrage::Adopter(s.clone());
        }
    }
    if !choisi && trouves.len() == 1 {
        return ChoixAuDemarrage::Adopter(trouves[0].clone());
    }
    ChoixAuDemarrage::Proposer
}

/// Comment le démon mDNS écoute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interfaces {
    /// Toutes les interfaces : l'usage normal.
    Toutes,
    /// La boucle locale seule : les tests, qui ne doivent rien annoncer ni
    /// chercher sur le réseau de la machine qui les exécute.
    BoucleSeule,
}

/// Parcourt le réseau pendant `duree` et rend les serveurs trouvés.
///
/// Bloquant : à appeler hors du fil asynchrone (`spawn_blocking`).
pub fn decouvrir(duree: Duration, interfaces: Interfaces) -> Result<Vec<ServeurTrouve>, String> {
    use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent};

    let demon = ServiceDaemon::new().map_err(|e| format!("démon mDNS : {e}"))?;
    if interfaces == Interfaces::BoucleSeule {
        demon
            .disable_interface(IfKind::All)
            .map_err(|e| format!("mDNS : {e}"))?;
        demon
            .enable_interface(IfKind::LoopbackV4)
            .map_err(|e| format!("mDNS : {e}"))?;
    }
    let recepteur = demon
        .browse(SERVICE)
        .map_err(|e| format!("parcours mDNS : {e}"))?;

    let fin = Instant::now() + duree;
    let mut trouves = Vec::new();
    loop {
        let reste = fin.saturating_duration_since(Instant::now());
        if reste.is_zero() {
            break;
        }
        match recepteur.recv_timeout(reste) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let adresses: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .collect();
                match serveur_depuis(
                    info.get_fullname(),
                    info.get_hostname(),
                    &adresses,
                    info.get_port(),
                    info.get_property_val_str("version"),
                ) {
                    Some(s) => {
                        tracing::info!(nom = %s.nom, url = %s.url, "serveur_tune_trouve");
                        trouves.push(s);
                    }
                    None => tracing::info!(
                        service = info.get_fullname(),
                        ?adresses,
                        "serveur_tune_sans_adresse_utilisable"
                    ),
                }
            }
            Ok(_) => {}
            // Délai écoulé, ou démon arrêté : on rend ce qu'on a.
            Err(_) => break,
        }
    }
    let _ = demon.stop_browse(SERVICE);
    let _ = demon.shutdown();
    Ok(fusionner(trouves))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn serveur(cle: &str, nom: &str, url: &str) -> ServeurTrouve {
        ServeurTrouve {
            nom: nom.into(),
            url: url.into(),
            hote: "h.local".into(),
            version: None,
            cle: cle.into(),
        }
    }

    #[test]
    fn le_nom_d_instance_perd_le_type_et_ses_echappements() {
        assert_eq!(
            nom_d_instance("Tune (salon)._tune-server._tcp.local."),
            "Tune (salon)"
        );
        assert_eq!(
            nom_d_instance("Tune\\032Mac\\.mini._tune-server._tcp.local."),
            "Tune Mac.mini"
        );
        // Un nom inattendu reste tel quel plutôt que d'être tronqué.
        assert_eq!(nom_d_instance("autre"), "autre");
    }

    #[test]
    fn l_adresse_du_lan_l_emporte() {
        let lan = v4(192, 168, 1, 18);
        let docker = v4(172, 17, 0, 1); // privée aussi, mais pont Docker
        let boucle = v4(127, 0, 0, 1);
        let lien = v4(169, 254, 3, 4);
        let fe80: IpAddr = "fe80::1".parse().unwrap();
        let v6: IpAddr = "2a01:e0a::5".parse().unwrap();
        assert_eq!(meilleure_adresse(&[boucle, fe80, lan]), Some(lan));
        assert_eq!(meilleure_adresse(&[v6, lien]), Some(lien));
        assert_eq!(meilleure_adresse(&[v6, boucle]), Some(v6));
        // L'ordre d'arrivée ne compte pas, le LAN passe devant le pont Docker.
        assert_eq!(meilleure_adresse(&[docker, lan]), Some(lan));
        assert_eq!(meilleure_adresse(&[lan, docker]), Some(lan));
        assert_eq!(
            meilleure_adresse(&[docker, v4(10, 0, 0, 5)]),
            Some(v4(10, 0, 0, 5))
        );
        // Le serveur sur CETTE machine, qui ne connaît que la boucle.
        assert_eq!(meilleure_adresse(&[boucle]), Some(boucle));
        // Rien d'utilisable par une URL http.
        assert_eq!(meilleure_adresse(&[fe80]), None);
        assert_eq!(
            meilleure_adresse(&[IpAddr::V4(Ipv4Addr::UNSPECIFIED)]),
            None
        );
        assert_eq!(meilleure_adresse(&[]), None);
    }

    #[test]
    fn l_url_met_l_ipv6_entre_crochets() {
        assert_eq!(url_de(v4(10, 0, 0, 2), 8888), "http://10.0.0.2:8888");
        assert_eq!(
            url_de(IpAddr::V6(Ipv6Addr::LOCALHOST), 8888),
            "http://[::1]:8888"
        );
    }

    #[test]
    fn un_service_sans_adresse_ni_port_n_est_pas_un_serveur() {
        let ok = serveur_depuis(
            "Tune (nas)._tune-server._tcp.local.",
            "nas.local.",
            &[v4(192, 168, 1, 29)],
            8888,
            Some(" 1.0.0-rc13 "),
        )
        .unwrap();
        assert_eq!(ok.nom, "Tune (nas)");
        assert_eq!(ok.url, "http://192.168.1.29:8888");
        assert_eq!(ok.hote, "nas.local");
        assert_eq!(ok.version.as_deref(), Some("1.0.0-rc13"));
        assert!(serveur_depuis("x", "h", &[], 8888, None).is_none());
        assert!(serveur_depuis("x", "h", &[v4(192, 168, 1, 2)], 0, None).is_none());
    }

    #[test]
    fn la_liste_est_sans_doublon_et_triee() {
        let l = fusionner(vec![
            serveur("b", "Tune (salon)", "http://192.168.1.3:8888"),
            serveur("a", "tune (bureau)", "http://192.168.1.2:8888"),
            serveur("b", "Tune (salon)", "http://192.168.1.4:8888"),
        ]);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].nom, "tune (bureau)");
        // La dernière résolution l'emporte (nouvelle adresse).
        assert_eq!(l[1].url, "http://192.168.1.4:8888");
    }

    #[test]
    fn le_choix_au_demarrage() {
        let a = serveur("a", "A", "http://192.168.1.2:8888");
        let b = serveur("b", "B", "http://192.168.1.3:8888");
        use ChoixAuDemarrage::*;
        // 1. Ce qui répond est gardé, même avec d'autres serveurs en vue.
        assert_eq!(
            choisir_au_demarrage(None, false, true, &[a.clone(), b.clone()]),
            Garder
        );
        // 3. Premier lancement, serveur sur une autre machine (fil 2968).
        assert_eq!(
            choisir_au_demarrage(None, false, false, std::slice::from_ref(&a)),
            Adopter(a.clone())
        );
        // 4. Plusieurs serveurs : on demande.
        assert_eq!(
            choisir_au_demarrage(None, false, false, &[a.clone(), b.clone()]),
            Proposer
        );
        // 4. Aucun : la liste vide et la saisie manuelle.
        assert_eq!(choisir_au_demarrage(None, false, false, &[]), Proposer);
        // 2. Le serveur choisi a changé d'adresse : on le suit.
        assert_eq!(
            choisir_au_demarrage(Some("b"), true, false, &[a.clone(), b.clone()]),
            Adopter(b.clone())
        );
        // 4. Le serveur choisi a disparu : jamais de bascule silencieuse vers
        // un autre, même s'il est seul.
        assert_eq!(
            choisir_au_demarrage(Some("z"), true, false, std::slice::from_ref(&a)),
            Proposer
        );
        // Adresse tapée à la main qui ne répond plus : on demande aussi.
        assert_eq!(
            choisir_au_demarrage(None, true, false, std::slice::from_ref(&a)),
            Proposer
        );
    }

    /// Le parcours réel, contre un FAUX serveur Tune annoncé par un second
    /// démon mDNS, sur la boucle locale seulement : rien ne sort sur le réseau
    /// de la machine qui exécute le test.
    #[test]
    fn un_faux_serveur_annonce_est_trouve() {
        use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

        let annonceur = ServiceDaemon::new().expect("démon annonceur");
        annonceur.disable_interface(IfKind::All).unwrap();
        annonceur.enable_interface(IfKind::LoopbackV4).unwrap();
        // Un nom unique : deux exécutions simultanées ne se mélangent pas.
        let instance = format!(
            "Faux Tune {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros()
        );
        let info = ServiceInfo::new(
            SERVICE,
            &instance,
            "faux-tune.local.",
            "127.0.0.1",
            18888,
            &[("version", "9.9.9"), ("path", "/api/v1")][..],
        )
        .unwrap();
        annonceur.register(info).unwrap();

        let trouves = decouvrir(DUREE_PARCOURS, Interfaces::BoucleSeule).expect("parcours");
        let _ = annonceur.shutdown();

        let s = trouves
            .iter()
            .find(|s| s.nom == instance)
            .unwrap_or_else(|| panic!("faux serveur absent de {trouves:?}"));
        assert_eq!(s.url, "http://127.0.0.1:18888");
        assert_eq!(s.version.as_deref(), Some("9.9.9"));
        assert_eq!(s.hote, "faux-tune.local");
    }

    /// Le parcours sur le VRAI réseau de la machine : à lancer à la main
    /// (`cargo test -- --ignored --nocapture le_reseau_reel`) pour voir ce
    /// que l'app trouverait. Jamais en CI : le runner n'a pas de serveur Tune.
    #[test]
    #[ignore]
    fn le_reseau_reel() {
        let trouves = decouvrir(DUREE_PARCOURS, Interfaces::Toutes).expect("parcours");
        for s in &trouves {
            println!("{} — {} ({:?}) [{}]", s.nom, s.url, s.version, s.hote);
        }
        println!("{} serveur(s)", trouves.len());
    }

    /// macOS n'accorde « Réseau local » qu'aux apps qui déclarent ce qu'elles
    /// cherchent : sans `_tune-server._tcp` dans `NSBonjourServices`, le
    /// parcours échoue en silence sur un Mac récent (#4949 côté serveur).
    #[test]
    fn info_plist_declare_le_reseau_local_et_le_service() {
        let plist = include_str!("../Info.plist");
        assert!(plist.contains("<key>NSLocalNetworkUsageDescription</key>"));
        assert!(plist.contains("<key>NSBonjourServices</key>"));
        assert!(
            plist.contains(&format!("<string>{TYPE_BONJOUR}</string>")),
            "NSBonjourServices doit déclarer {TYPE_BONJOUR}"
        );
        assert!(SERVICE.starts_with(TYPE_BONJOUR));
    }
}
