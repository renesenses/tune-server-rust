//! La sortie LOCALE lit le flux interne de Tune par la boucle locale (#5639).
//!
//! Sous Windows, une radio lancée sur une sortie locale recevait un
//! `403 Forbidden` chaque fois que la sortie demandait à Tune son propre flux,
//! `http://<ip-lan>:8888/stream/<id>.wav`. Aucun chemin du serveur de flux ne
//! produit ce statut. Hypothèse retenue, non mesurée sur le poste touché : un
//! AUTRE programme répond sur `<ip-lan>:8888`. Windows laisse un programme
//! écouter sur une adresse précise quand Tune écoute sur toutes (`[::]` ou
//! `0.0.0.0`) ; la connexion vers l'adresse LAN arrive alors chez lui.
//!
//! La sortie locale tourne sur la MÊME machine que le serveur : elle n'a aucun
//! besoin de l'adresse LAN. Ce module réécrit donc l'adresse du flux interne
//! vers `127.0.0.1`, au même port. L'URL est construite par
//! `AudioStreamer::get_stream_url` avec le port du serveur, celui que
//! `bootstrap` a réellement lié (sinon le processus s'arrête) : garder le port
//! de l'URL, c'est garder le port réel.
//!
//! Ce qui n'est PAS touché : les URL données aux renderers réseau (DLNA,
//! Chromecast, AirPlay, OAAT…). Elles sont construites au même endroit mais
//! ne passent jamais par ici : seule la sortie locale ouvre ses flux par
//! `LecteurHttpAnnulable::ouvrir`, et c'est là que la réécriture s'applique.
//! Un renderer, lui, a besoin de l'adresse LAN.
//!
//! La réécriture ne vise que :
//! - une URL `http://` dont le chemin commence par `/stream/` ;
//! - dont l'hôte est une adresse IP de CETTE machine (un pair Tune distant
//!   garde son adresse : `remote_proxy` sert aussi des `/stream/`).
//!
//! Un nom d'hôte n'est jamais résolu ici : il reste tel quel.
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// Où la sortie locale va réellement lire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AdresseDeLecture {
    /// L'URL à ouvrir.
    pub url: String,
    /// L'URL désigne le flux interne de Tune (`/stream/…` sur cette machine).
    pub flux_interne: bool,
}

/// Le chemin du flux interne, tel que `AudioStreamer::get_stream_url` le pose.
const CHEMIN_DU_FLUX: &str = "/stream/";

/// Une adresse IP appartient-elle à cette machine ?
///
/// Une boucle est toujours locale. Pour les autres, on demande au système de
/// lier une socket UDP sur cette adresse, port 0 : il ne l'accepte que pour
/// une adresse portée par une interface de la machine. Rien n'est émis.
pub(super) fn est_une_adresse_de_cette_machine(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok()
}

/// La construction de l'adresse, sans réseau : `est_locale` dit si une IP est
/// à cette machine. Fonction pure, pour les épreuves.
pub(super) fn adresse_de_lecture_avec(
    url: &str,
    est_locale: impl Fn(IpAddr) -> bool,
) -> AdresseDeLecture {
    let telle_quelle = AdresseDeLecture {
        url: url.to_string(),
        flux_interne: false,
    };
    let Ok(mut analysee) = reqwest::Url::parse(url) else {
        return telle_quelle;
    };
    if analysee.scheme() != "http" || !analysee.path().starts_with(CHEMIN_DU_FLUX) {
        return telle_quelle;
    }
    // `host_str` rend une IPv6 entre crochets ; un nom d'hôte ne se lit pas
    // comme une IP et reste tel quel.
    let Some(ip) = analysee
        .host_str()
        .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|h| h.parse::<IpAddr>().ok())
    else {
        return telle_quelle;
    };
    if !est_locale(ip) {
        return telle_quelle;
    }
    if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return AdresseDeLecture {
            url: url.to_string(),
            flux_interne: true,
        };
    }
    // Le port reste celui de l'URL : c'est le port réel du serveur.
    if analysee
        .set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .is_err()
    {
        return telle_quelle;
    }
    AdresseDeLecture {
        url: analysee.to_string(),
        flux_interne: true,
    }
}

/// L'adresse que la sortie locale ouvre pour `url`.
pub(super) fn adresse_de_lecture(url: &str) -> AdresseDeLecture {
    adresse_de_lecture_avec(url, est_une_adresse_de_cette_machine)
}

/// Faut-il dire, au journal, que le flux interne a été refusé ?
///
/// Le flux interne de Tune ne répond jamais 403 : un 403 dit que quelqu'un
/// d'autre a répondu. La ligne nomme l'adresse jointe, pour qu'un rapport de
/// bug suffise à le voir.
pub(super) fn signaler_le_refus(statut: u16, cible: &AdresseDeLecture) -> bool {
    statut == 403 && cible.flux_interne
}

/// L'hôte et le port réellement joints, pour la ligne de journal.
pub(super) fn adresse_jointe(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port_or_known_default()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => url.to_string(),
        },
        Err(_) => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une machine dont l'adresse « LAN » est prise dans une plage de
    /// documentation (RFC 5737 / RFC 4193), jamais une adresse réelle.
    fn lan(ip: IpAddr) -> bool {
        ip == "198.51.100.65".parse::<IpAddr>().unwrap()
            || ip == "fd00::65".parse::<IpAddr>().unwrap()
            || ip.is_loopback()
    }

    #[test]
    fn le_flux_interne_sur_l_adresse_lan_passe_par_la_boucle_au_meme_port() {
        let a = adresse_de_lecture_avec("http://198.51.100.65:8888/stream/abc-123.wav", lan);
        assert_eq!(a.url, "http://127.0.0.1:8888/stream/abc-123.wav");
        assert!(a.flux_interne);
    }

    #[test]
    fn le_port_reel_est_conserve_quel_qu_il_soit() {
        let a = adresse_de_lecture_avec("http://198.51.100.65:9123/stream/x.flac?seek=10", lan);
        assert_eq!(a.url, "http://127.0.0.1:9123/stream/x.flac?seek=10");
    }

    #[test]
    fn une_adresse_ipv6_de_la_machine_passe_aussi_par_la_boucle() {
        let a = adresse_de_lecture_avec("http://[fd00::65]:8888/stream/x.wav", lan);
        assert_eq!(a.url, "http://127.0.0.1:8888/stream/x.wav");
        assert!(a.flux_interne);
    }

    #[test]
    fn une_url_deja_en_boucle_reste_telle_quelle_et_compte_comme_interne() {
        let a = adresse_de_lecture_avec("http://127.0.0.1:8888/stream/x.wav", lan);
        assert_eq!(a.url, "http://127.0.0.1:8888/stream/x.wav");
        assert!(a.flux_interne);
    }

    #[test]
    fn un_pair_tune_distant_garde_son_adresse() {
        let a = adresse_de_lecture_avec("http://198.51.100.99:8888/stream/x.wav", lan);
        assert_eq!(a.url, "http://198.51.100.99:8888/stream/x.wav");
        assert!(!a.flux_interne);
    }

    #[test]
    fn hors_du_flux_interne_rien_ne_change() {
        for url in [
            "http://198.51.100.65:8888/api/v1/zones",
            "https://198.51.100.65:8888/stream/x.wav",
            "http://stream.example.com:8080/radio.aacp",
            "http://tune.local:8888/stream/x.wav",
            "pas une url",
        ] {
            let a = adresse_de_lecture_avec(url, lan);
            assert_eq!(a.url, url, "{url}");
            assert!(!a.flux_interne, "{url}");
        }
    }

    #[test]
    fn la_boucle_est_de_cette_machine_une_adresse_de_documentation_non() {
        assert!(est_une_adresse_de_cette_machine(
            "127.0.0.1".parse().unwrap()
        ));
        // 192.0.2.0/24 (TEST-NET-1) n'est portée par aucune interface.
        assert!(!est_une_adresse_de_cette_machine(
            "192.0.2.77".parse().unwrap()
        ));
        assert!(!est_une_adresse_de_cette_machine(
            "0.0.0.0".parse().unwrap()
        ));
    }

    #[test]
    fn un_403_du_flux_interne_se_signale_et_nomme_l_adresse_jointe() {
        let interne = adresse_de_lecture_avec("http://198.51.100.65:8888/stream/x.wav", lan);
        assert!(signaler_le_refus(403, &interne));
        assert!(!signaler_le_refus(404, &interne));
        let externe = adresse_de_lecture_avec("http://stream.example.com/x.aacp", lan);
        assert!(!signaler_le_refus(403, &externe));
        assert_eq!(adresse_jointe(&interne.url), "127.0.0.1:8888");
        assert_eq!(
            adresse_jointe("http://198.51.100.65:8888/stream/x.wav"),
            "198.51.100.65:8888"
        );
    }

    /// Répond `reponse` à chaque connexion, sur un fil détaché.
    fn repondeur(ecoute: std::net::TcpListener, reponse: &'static str) {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for flux in ecoute.incoming() {
                let Ok(mut flux) = flux else { return };
                let mut tampon = [0u8; 2048];
                let _ = flux.read(&mut tampon);
                let _ = flux.write_all(reponse.as_bytes());
            }
        });
    }

    /// Le scénario de #5639, rejoué : Tune écoute sur la boucle, un AUTRE
    /// programme tient l'adresse LAN au même port et répond 403. La sortie
    /// locale doit joindre Tune.
    #[test]
    fn un_autre_programme_sur_l_adresse_lan_n_intercepte_plus_le_flux_interne() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let Some(ip_lan) = crate::discovery::ssdp::get_local_ip() else {
            eprintln!("aucune adresse LAN sur cette machine : épreuve sautée");
            return;
        };
        let tune = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tune.local_addr().unwrap().port();
        let Ok(autre) = std::net::TcpListener::bind((ip_lan, port)) else {
            eprintln!("{ip_lan}:{port} indisponible : épreuve sautée");
            return;
        };
        repondeur(
            tune,
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 4\r\nConnection: close\r\n\r\nRIFF",
        );
        repondeur(
            autre,
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let url = format!("http://{ip_lan}:{port}/stream/abc.wav");
        let lecteur = super::super::lecture_http::LecteurHttpAnnulable::ouvrir(
            &url,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("ouverture HTTP");
        assert_eq!(
            lecteur.status().as_u16(),
            200,
            "la sortie locale a joint l'autre programme sur {ip_lan}:{port} au lieu de Tune"
        );
    }
}
