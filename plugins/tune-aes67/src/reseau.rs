//! Les sockets UDP de l'émetteur : interface, TTL multicast, DSCP.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};
use tracing::warn;

/// Ouvre un socket d'émission vers `destination`.
///
/// - multicast : TTL multicast, interface d'émission si elle est donnée, et
///   boucle locale ACTIVE (un récepteur logiciel sur la même machine doit
///   entendre le flux) ;
/// - DSCP posé dans l'octet TOS (`dscp << 2`) ; un refus du système est
///   journalisé, pas fatal ;
/// - `connecter` : le socket média est connecté (un seul destinataire, `send`
///   sans adresse) ; celui de SAP ne l'est pas.
pub fn ouvrir_socket(
    destination: SocketAddrV4,
    interface: Option<Ipv4Addr>,
    ttl: u8,
    dscp: u8,
    connecter: bool,
) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let locale = SocketAddrV4::new(interface.unwrap_or(Ipv4Addr::UNSPECIFIED), 0);
    socket.bind(&locale.into())?;
    if destination.ip().is_multicast() {
        socket.set_multicast_ttl_v4(u32::from(ttl))?;
        if let Some(interface) = interface {
            socket.set_multicast_if_v4(&interface)?;
        }
        socket.set_multicast_loop_v4(true)?;
    }
    if let Err(e) = socket.set_tos(u32::from(dscp & 0x3f) << 2) {
        warn!(error = %e, dscp, "aes67_dscp_refuse");
    }
    if connecter {
        socket.connect(&destination.into())?;
    }
    Ok(socket.into())
}

/// L'adresse IPv4 que l'émetteur annonce (`o=` du SDP, source SAP) :
/// l'interface choisie, sinon celle que le système prendrait pour joindre
/// `destination`. Aucun paquet ne part : `connect` sur UDP ne fait que
/// consulter la table de routage.
pub fn adresse_d_origine(destination: SocketAddrV4, interface: Option<Ipv4Addr>) -> Ipv4Addr {
    if let Some(i) = interface {
        return i;
    }
    let essai = || -> io::Result<Ipv4Addr> {
        let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        s.connect(destination)?;
        match s.local_addr()? {
            std::net::SocketAddr::V4(v4) => Ok(*v4.ip()),
            std::net::SocketAddr::V6(_) => Err(io::Error::other("IPv6")),
        }
    };
    essai().unwrap_or_else(|e| {
        warn!(error = %e, %destination, "aes67_origine_inconnue");
        Ipv4Addr::UNSPECIFIED
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_boucle_locale_se_designe_elle_meme() {
        let d = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5004);
        assert_eq!(adresse_d_origine(d, None), Ipv4Addr::LOCALHOST);
        let i = Ipv4Addr::new(10, 1, 2, 3);
        assert_eq!(
            adresse_d_origine(d, Some(i)),
            i,
            "l'interface choisie prime"
        );
    }

    #[test]
    fn un_socket_unicast_connecte_envoie_sur_la_boucle_locale() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(d) = ecoute.local_addr().unwrap() else {
            panic!("IPv4 attendu");
        };
        let s = ouvrir_socket(d, Some(Ipv4Addr::LOCALHOST), 16, 34, true).unwrap();
        s.send(b"x").unwrap();
        let mut t = [0u8; 4];
        ecoute
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        assert_eq!(ecoute.recv(&mut t).unwrap(), 1);
    }
}
