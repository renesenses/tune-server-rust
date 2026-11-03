//! #6885 — l'URI RTSP et le SDP de l'ANNOUNCE disent au récepteur AirPlay 1
//! l'adresse LOCALE réelle de la connexion et un identifiant de session
//! stable, au lieu de `rtsp://127.0.0.1/1` et `o=iTunes 1 O IN IP4 127.0.0.1`.
//!
//! Deux niveaux de preuve :
//! - l'identité est une fonction pure de (id, ip locale, ip récepteur) : des
//!   adresses LAN et IPv6 y sont injectées, sans réseau ;
//! - un faux récepteur écoute sur une adresse NON boucle de la machine quand
//!   elle en a une (repli sur la boucle sinon) ; il relève chaque requête et
//!   le SDP, et les confronte à l'adresse qu'il voit pour l'émetteur.
//!
//! Aucun appareil réel, aucun son.

use super::*;
use std::net::{IpAddr, Ipv4Addr};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const ID: u32 = 3_413_821_438;

fn ip(texte: &str) -> IpAddr {
    texte.parse().unwrap()
}

#[test]
fn identite_ipv4_lan_porte_l_adresse_locale_et_l_identifiant() {
    let identite = IdentiteSessionRaop::new(ID, ip("192.168.1.10"), ip("192.168.1.20"));
    let sdp = identite.sdp_announce();

    assert_eq!(identite.uri(), "rtsp://192.168.1.10/3413821438");
    assert!(
        sdp.contains("\r\no=iTunes 3413821438 0 IN IP4 192.168.1.10\r\n"),
        "o= : identifiant de session, version 0 (chiffre), adresse locale ; obtenu :\n{sdp}"
    );
    assert!(
        sdp.contains("\r\nc=IN IP4 192.168.1.20\r\n"),
        "c= : adresse du récepteur ; obtenu :\n{sdp}"
    );
    assert!(!sdp.contains("127.0.0.1"), "obtenu :\n{sdp}");
    assert!(sdp.starts_with("v=0\r\n") && sdp.contains("\r\nm=audio 0 RTP/AVP 96\r\n"));
}

#[test]
fn identite_ipv6_annonce_ip6_et_crochets_dans_l_uri() {
    let identite = IdentiteSessionRaop::new(ID, ip("fe80::217:f2ff:fe0f:e0f6"), ip("fe80::2"));
    let sdp = identite.sdp_announce();

    assert_eq!(
        identite.uri(),
        "rtsp://[fe80::217:f2ff:fe0f:e0f6]/3413821438"
    );
    assert!(sdp.contains("\r\no=iTunes 3413821438 0 IN IP6 fe80::217:f2ff:fe0f:e0f6\r\n"));
    assert!(sdp.contains("\r\nc=IN IP6 fe80::2\r\n"));
}

#[test]
fn identite_ipv4_vue_par_une_socket_double_pile_reste_ip4() {
    let locale = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 10).to_ipv6_mapped());
    let recepteur = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped());
    let identite = IdentiteSessionRaop::new(ID, locale, recepteur);

    assert_eq!(identite.uri(), "rtsp://192.168.1.10/3413821438");
    assert!(
        identite
            .sdp_announce()
            .contains("o=iTunes 3413821438 0 IN IP4 192.168.1.10\r\n")
    );
    assert!(
        identite
            .sdp_announce()
            .contains("c=IN IP4 192.168.1.20\r\n")
    );
}

#[test]
fn deux_connexions_tirent_deux_identifiants() {
    // 1 chance sur 2^32 de collision : un échec ici est un tirage constant.
    assert_ne!(
        IdentiteSessionRaop::tirer_id(),
        IdentiteSessionRaop::tirer_id()
    );
}

/// Une adresse non boucle de la machine, celle que le noyau choisirait pour
/// sortir vers TEST-NET-1 (RFC 5737). `connect` sur UDP n'envoie rien.
fn adresse_non_boucle() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let locale = socket.local_addr().ok()?.ip();
    (!locale.is_loopback() && !locale.is_unspecified()).then_some(locale)
}

/// Un faux récepteur qui relève chaque requête (ligne de requête, en-têtes,
/// corps). Il répond comme le récepteur de la capture de #6885 (SETUP avec
/// `Session: 1`) et refuse RECORD par 453 pour clore le dialogue sans flux :
/// ANNOUNCE, SETUP, RECORD puis le TEARDOWN de rattrapage (#2217).
async fn faux_recepteur_releveur(
    adresse: IpAddr,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<(std::net::SocketAddr, Vec<String>)>,
) {
    let listener = tokio::net::TcpListener::bind((adresse, 0)).await.unwrap();
    let ecoute = listener.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        let (socket, emetteur) = listener.accept().await.unwrap();
        let mut lecteur = BufReader::new(socket);
        let mut requetes = Vec::new();
        loop {
            let mut requete = String::new();
            if lecteur.read_line(&mut requete).await.unwrap_or(0) == 0 {
                break;
            }
            let methode = requete.split_whitespace().next().unwrap_or("").to_string();
            let mut taille = 0usize;
            loop {
                let mut entete = String::new();
                if lecteur.read_line(&mut entete).await.unwrap_or(0) == 0 {
                    break;
                }
                requete.push_str(&entete);
                let entete = entete.trim_end();
                if entete.is_empty() {
                    break;
                }
                if let Some((cle, valeur)) = entete.split_once(':')
                    && cle.trim().eq_ignore_ascii_case("Content-Length")
                {
                    taille = valeur.trim().parse().unwrap_or(0);
                }
            }
            if taille > 0 {
                let mut corps = vec![0_u8; taille];
                lecteur.read_exact(&mut corps).await.unwrap();
                requete.push_str(&String::from_utf8_lossy(&corps));
            }
            requetes.push(requete);
            let reponse = match methode.as_str() {
                "SETUP" => "RTSP/1.0 200 OK\r\nCSeq: 2\r\nSession: 1\r\nContent-Length: 0\r\n\r\n",
                "RECORD" => {
                    "RTSP/1.0 453 Not Enough Bandwidth\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n"
                }
                _ => "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n",
            };
            lecteur
                .get_mut()
                .write_all(reponse.as_bytes())
                .await
                .unwrap();
        }
        (emetteur, requetes)
    });
    (ecoute, tache)
}

/// Le témoin de #6885 sur le dialogue réel : chaque requête porte
/// `rtsp://<adresse que le récepteur voit pour l'émetteur>/<id>`, le même id
/// partout, et le SDP de l'ANNOUNCE porte ce même id et ces adresses.
#[tokio::test]
async fn l_uri_et_le_sdp_portent_l_adresse_locale_de_la_connexion() {
    let non_boucle = adresse_non_boucle();
    let adresse = non_boucle.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    if non_boucle.is_none() {
        eprintln!("#6885 : aucune adresse non boucle, repli sur 127.0.0.1");
    }
    let (ecoute, recepteur) = faux_recepteur_releveur(adresse).await;
    let sortie = AirplayOutput::new(
        "Faux recepteur 6885".into(),
        format!("airplay-{}-{}", ecoute.ip(), ecoute.port()),
        ecoute.ip().to_string(),
        ecoute.port(),
    );
    let erreur = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sortie.play_media(&PlayMedia {
            url: "file:///dev/null",
            mime_type: "audio/wav",
            ..Default::default()
        }),
    )
    .await
    .expect("play_media ne doit pas rester suspendu")
    .expect_err("RECORD refusé : play_media doit rendre Err");
    assert!(erreur.contains("RECORD failed: 453"), "obtenu : {erreur}");

    let (emetteur, requetes) = tokio::time::timeout(std::time::Duration::from_secs(5), recepteur)
        .await
        .expect("le faux récepteur doit finir")
        .unwrap();
    let methodes: Vec<&str> = requetes
        .iter()
        .map(|r| r.split_whitespace().next().unwrap_or(""))
        .collect();
    assert_eq!(methodes, vec!["ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"]);

    let ip_emetteur = emetteur.ip().to_canonical();
    let prefixe = match ip_emetteur {
        IpAddr::V4(ip) => format!("rtsp://{ip}/"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/"),
    };
    let mut identifiants = Vec::new();
    for requete in &requetes {
        let uri = requete.split_whitespace().nth(1).unwrap_or("");
        let id = uri.strip_prefix(prefixe.as_str()).unwrap_or_else(|| {
            panic!("l'URI doit porter l'adresse que le récepteur voit ({ip_emetteur}) : {requete}")
        });
        assert!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
            "identifiant de session numérique attendu : {uri}"
        );
        identifiants.push(id.to_string());
    }
    assert!(
        identifiants.iter().all(|id| *id == identifiants[0]),
        "le même identifiant dans chaque requête : {identifiants:?}"
    );

    let famille = if ip_emetteur.is_ipv4() { "IP4" } else { "IP6" };
    let annonce = &requetes[0];
    let attendu_o = format!(
        "\r\no=iTunes {} 0 IN {famille} {ip_emetteur}\r\n",
        identifiants[0]
    );
    let attendu_c = format!("\r\nc=IN {famille} {}\r\n", ecoute.ip().to_canonical());
    assert!(
        annonce.contains(&attendu_o),
        "attendu {attendu_o:?} dans :\n{annonce}"
    );
    assert!(
        annonce.contains(&attendu_c),
        "attendu {attendu_c:?} dans :\n{annonce}"
    );
    if non_boucle.is_some() {
        for requete in &requetes {
            assert!(
                !requete.contains("127.0.0.1"),
                "connexion hors boucle : 127.0.0.1 n'a rien à faire ici :\n{requete}"
            );
        }
    }
    // La session donnée par le récepteur continue de suivre RECORD et TEARDOWN.
    assert!(requetes[2].contains("Session: 1\r\n") && requetes[3].contains("Session: 1\r\n"));
}
