//! Le transport PTP sur UDP/IPv4 (IEEE 1588 annexe C) : deux sockets, l'un
//! pour les messages d'événement (port 319 : Sync, Delay_Req), l'autre pour
//! les messages généraux (port 320 : Announce, Follow_Up, Delay_Resp).
//!
//! HORODATAGE LOGICIEL, en espace utilisateur : l'instant de réception est
//! lu sur l'horloge virtuelle juste après `recv`, l'instant d'émission juste
//! après `send`. Pas de `SO_TIMESTAMPING` ni de carte réseau PTP : ils
//! demandent une carte compatible ou des droits, et leur horloge (REALTIME ou
//! PHC) n'est pas celle qu'on asservit. Le prix : la latence de la pile et de
//! l'ordonnanceur entre dans la mesure, typiquement quelques dizaines de µs
//! de gigue sur un réseau calme, que le filtre de Kalman de `statime` moyenne.
//! L'étude (§ 3.2) vise ~100 µs : le récepteur absorbe la gigue dans son
//! tampon, seule la dérive à long terme doit être nulle.
//!
//! Les ports 319 et 320 sont PRIVILÉGIÉS sous Linux. Sans
//! `CAP_NET_BIND_SERVICE` (ou `net.ipv4.ip_unprivileged_port_start`), la
//! liaison échoue : le suiveur le dit dans son état et l'émetteur reste sur
//! l'horloge locale (étape E4 : capacités de l'unité systemd). Les tests et
//! le banc n'utilisent que des ports non privilégiés sur 127.0.0.1.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use statime::time::Time;
use tracing::warn;

use super::horloge_virtuelle::HorlogeVirtuelle;

/// Le groupe multicast PTP « primaire » (IEEE 1588 annexe C).
pub const GROUPE_PTP: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
pub const PORT_EVENEMENT: u16 = 319;
pub const PORT_GENERAL: u16 = 320;
/// DSCP recommandé par AES67 pour l'horloge : EF (46).
pub const DSCP_PTP: u8 = 46;

/// Où écouter, où envoyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigReseauPtp {
    pub liaison_evenement: SocketAddrV4,
    pub liaison_generale: SocketAddrV4,
    pub destination_evenement: SocketAddrV4,
    pub destination_generale: SocketAddrV4,
    /// Interface d'émission et d'abonnement multicast (`0.0.0.0` : celle
    /// que choisit le système).
    pub interface: Ipv4Addr,
}

impl ConfigReseauPtp {
    /// Le réseau AES67 : 224.0.1.129, ports 319 et 320.
    pub fn aes67(interface: Option<Ipv4Addr>) -> Self {
        Self {
            liaison_evenement: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT_EVENEMENT),
            liaison_generale: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT_GENERAL),
            destination_evenement: SocketAddrV4::new(GROUPE_PTP, PORT_EVENEMENT),
            destination_generale: SocketAddrV4::new(GROUPE_PTP, PORT_GENERAL),
            interface: interface.unwrap_or(Ipv4Addr::UNSPECIFIED),
        }
    }

    /// Banc d'essai : unicast sur `locale`, ports libres, vers un maître
    /// dont on connaît les deux adresses. Aucun droit requis.
    pub fn unicast(
        locale: Ipv4Addr,
        maitre_evenement: SocketAddrV4,
        maitre_general: SocketAddrV4,
    ) -> Self {
        Self {
            liaison_evenement: SocketAddrV4::new(locale, 0),
            liaison_generale: SocketAddrV4::new(locale, 0),
            destination_evenement: maitre_evenement,
            destination_generale: maitre_general,
            interface: locale,
        }
    }

    /// Le groupe à rejoindre, si les destinations sont multicast.
    fn groupe(&self) -> Option<Ipv4Addr> {
        let g = *self.destination_evenement.ip();
        g.is_multicast().then_some(g)
    }
}

/// D'où vient un message reçu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Canal {
    Evenement,
    General,
}

/// Un message reçu, horodaté à l'arrivée sur l'horloge virtuelle.
#[derive(Debug)]
pub struct Recu {
    pub canal: Canal,
    pub donnees: Vec<u8>,
    pub horodatage: Time,
}

fn ouvrir_un(liaison: SocketAddrV4, cfg: &ConfigReseauPtp) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Plusieurs auditeurs PTP sur la même machine (un ptp4l, PipeWire) :
    // chacun reçoit sa copie du multicast.
    socket.set_reuse_address(true)?;
    socket.bind(&liaison.into()).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            io::Error::new(
                e.kind(),
                format!(
                    "port {} refusé : les ports PTP 319 et 320 demandent \
                     CAP_NET_BIND_SERVICE ({e})",
                    liaison.port()
                ),
            )
        } else {
            io::Error::new(e.kind(), format!("liaison {liaison} : {e}"))
        }
    })?;
    if let Some(groupe) = cfg.groupe() {
        socket.join_multicast_v4(&groupe, &cfg.interface)?;
        if !cfg.interface.is_unspecified() {
            socket.set_multicast_if_v4(&cfg.interface)?;
        }
        // Ne pas s'entendre soi-même ; un TTL de 1 : PTP ne traverse pas de
        // routeur dans le profil AES67.
        socket.set_multicast_loop_v4(false)?;
        socket.set_multicast_ttl_v4(1)?;
    }
    if let Err(e) = socket.set_tos(u32::from(DSCP_PTP) << 2) {
        warn!(error = %e, "aes67_ptp_dscp_refuse");
    }
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket.into())
}

/// Les deux sockets ouverts.
pub struct TransportUdp {
    evenement: UdpSocket,
    general: UdpSocket,
    cfg: ConfigReseauPtp,
}

impl TransportUdp {
    pub fn ouvrir(cfg: ConfigReseauPtp) -> io::Result<Self> {
        let evenement = ouvrir_un(cfg.liaison_evenement, &cfg)?;
        let general = ouvrir_un(cfg.liaison_generale, &cfg)?;
        Ok(Self {
            evenement,
            general,
            cfg,
        })
    }

    /// Les adresses réellement liées (port 0 : celui que le système a pris).
    pub fn adresses_locales(&self) -> io::Result<(SocketAddrV4, SocketAddrV4)> {
        let v4 = |s: &UdpSocket| match s.local_addr()? {
            std::net::SocketAddr::V4(a) => Ok(a),
            std::net::SocketAddr::V6(_) => Err(io::Error::other("IPv6")),
        };
        Ok((v4(&self.evenement)?, v4(&self.general)?))
    }

    /// Envoie un message d'événement et rend l'instant d'émission (logiciel).
    pub fn envoyer_evenement(
        &self,
        donnees: &[u8],
        horloge: &HorlogeVirtuelle,
    ) -> io::Result<Time> {
        self.evenement
            .send_to(donnees, self.cfg.destination_evenement)?;
        Ok(statime::Clock::now(horloge))
    }

    pub fn envoyer_general(&self, donnees: &[u8]) -> io::Result<()> {
        self.general
            .send_to(donnees, self.cfg.destination_generale)?;
        Ok(())
    }

    /// Un fil de réception par socket. Chaque message est horodaté AVANT
    /// d'être passé au fil PTP, pour que l'attente dans le canal n'entre pas
    /// dans la mesure.
    pub fn lancer_la_reception(
        &self,
        horloge: HorlogeVirtuelle,
        vers: Sender<Recu>,
        arret: Arc<AtomicBool>,
    ) -> io::Result<Vec<JoinHandle<()>>> {
        let mut fils = Vec::new();
        for (canal, socket) in [
            (Canal::Evenement, self.evenement.try_clone()?),
            (Canal::General, self.general.try_clone()?),
        ] {
            let horloge = horloge.clone();
            let vers = vers.clone();
            let arret = arret.clone();
            let nom = match canal {
                Canal::Evenement => "aes67-ptp-319",
                Canal::General => "aes67-ptp-320",
            };
            fils.push(
                std::thread::Builder::new()
                    .name(nom.into())
                    .spawn(move || {
                        let mut tampon = [0u8; 1500];
                        while !arret.load(Ordering::Acquire) {
                            match socket.recv(&mut tampon) {
                                Ok(n) => {
                                    let horodatage = statime::Clock::now(&horloge);
                                    let recu = Recu {
                                        canal,
                                        donnees: tampon[..n].to_vec(),
                                        horodatage,
                                    };
                                    if vers.send(recu).is_err() {
                                        break;
                                    }
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                    ) => {}
                                Err(e) => {
                                    warn!(error = %e, ?canal, "aes67_ptp_reception_refusee");
                                    std::thread::sleep(Duration::from_millis(100));
                                }
                            }
                        }
                    })?,
            );
        }
        Ok(fils)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_profil_aes67_ecoute_319_et_320_sur_224_0_1_129() {
        let c = ConfigReseauPtp::aes67(None);
        assert_eq!(c.liaison_evenement.port(), 319);
        assert_eq!(c.liaison_generale.port(), 320);
        assert_eq!(c.destination_evenement, "224.0.1.129:319".parse().unwrap());
        assert_eq!(c.destination_generale, "224.0.1.129:320".parse().unwrap());
        assert_eq!(c.groupe(), Some(GROUPE_PTP));
    }

    #[test]
    fn le_banc_unicast_ne_demande_aucun_droit() {
        let m = "127.0.0.1:9".parse().unwrap();
        let c = ConfigReseauPtp::unicast(Ipv4Addr::LOCALHOST, m, m);
        assert_eq!(c.groupe(), None);
        let t = TransportUdp::ouvrir(c).unwrap();
        let (e, g) = t.adresses_locales().unwrap();
        assert!(e.port() >= 1024 && g.port() >= 1024 && e != g);
    }
}
