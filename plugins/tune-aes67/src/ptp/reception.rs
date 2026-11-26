//! Horodatage logiciel des datagrammes PTP. Le temps civil du noyau est
//! converti en un instant monotone avant toute conversion à l'horloge PTP.
use std::io;
use std::net::{SocketAddrV4, UdpSocket};
use std::time::Instant;

#[cfg(target_os = "linux")]
use std::time::{Duration, SystemTime};

/// Le compteur d’identifiants suit les envois réussis sur le socket d’événements.
/// L’appelant sérialise ces envois, y compris la lecture de leur reçu.
pub(super) struct Emission {
    #[cfg(target_os = "linux")]
    noyau: Option<(Correspondance, u32)>,
}

impl Emission {
    pub(super) fn activer(socket: &UdpSocket) -> Self {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::socket::{TimestampingFlag as F, setsockopt, sockopt::Timestamping};
            let flags = F::SOF_TIMESTAMPING_SOFTWARE
                | F::SOF_TIMESTAMPING_RX_SOFTWARE
                | F::SOF_TIMESTAMPING_TX_SOFTWARE
                | F::SOF_TIMESTAMPING_OPT_ID
                | F::SOF_TIMESTAMPING_OPT_TSONLY;
            let noyau = match setsockopt(socket, Timestamping, &flags) {
                Ok(()) => Some((Correspondance::new(), 0)),
                Err(e) => {
                    tracing::warn!(error = %e, "aes67_ptp_horodatage_noyau_indisponible");
                    None
                }
            };
            Self { noyau }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = socket;
            Self {}
        }
    }

    pub(super) fn au_noyau(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.noyau.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub(super) fn envoyer(
        &mut self,
        socket: &UdpSocket,
        donnees: &[u8],
        dest: SocketAddrV4,
    ) -> io::Result<Instant> {
        let avant = Instant::now();
        socket.send_to(donnees, dest)?;
        let apres = Instant::now();
        #[cfg(target_os = "linux")]
        if let Some((rep, prochain)) = &mut self.noyau {
            use nix::sys::socket::MsgFlags;
            let attendu = *prochain;
            *prochain = prochain.wrapping_add(1);
            // Un pilote peut remettre un reçu après le retour de send_to. Ne
            // jamais attribuer le reçu d’un envoi précédent au paquet courant.
            while let Ok((_, date, id)) = lire_message(
                socket,
                &mut [],
                MsgFlags::MSG_ERRQUEUE | MsgFlags::MSG_DONTWAIT,
            ) {
                if id == Some(attendu) {
                    if let Some(date) = date {
                        return rep.convertir(date, Repere::lire()).ok_or_else(|| {
                            io::Error::other("changement d’époque pendant l’envoi PTP")
                        });
                    }
                    break;
                }
            }
        }
        // Aucun reçu disponible à la remise du paquet. Encadrer l’appel
        // système, sans bloquer la boucle PTP en attendant le pilote.
        Ok(avant + (apres - avant) / 2)
    }
}

pub(super) struct Reception<'a> {
    socket: &'a UdpSocket,
    #[cfg(target_os = "linux")]
    correspondance: Option<Correspondance>,
}

impl<'a> Reception<'a> {
    pub(super) fn new(socket: &'a UdpSocket, noyau: bool) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = noyau;
        Self {
            socket,
            #[cfg(target_os = "linux")]
            correspondance: noyau.then(Correspondance::new),
        }
    }

    pub(super) fn recevoir(&mut self, tampon: &mut [u8]) -> io::Result<(usize, Option<Instant>)> {
        #[cfg(target_os = "linux")]
        if let Some(rep) = &mut self.correspondance {
            let (n, date, _) =
                lire_message(self.socket, tampon, nix::sys::socket::MsgFlags::empty())?;
            // Le socket peut contenir un paquet reçu avant son paramétrage.
            let arrivee = match date {
                Some(date) => rep.convertir(date, Repere::lire()),
                None => Some(Instant::now()),
            };
            return Ok((n, arrivee));
        }
        let n = self.socket.recv(tampon)?;
        Ok((n, Some(Instant::now())))
    }
}

#[cfg(target_os = "linux")]
fn lire_message(
    socket: &UdpSocket,
    tampon: &mut [u8],
    flags: nix::sys::socket::MsgFlags,
) -> io::Result<(usize, Option<SystemTime>, Option<u32>)> {
    use nix::sys::socket::{ControlMessageOwned, cmsg_space, recvmsg};
    use std::{io::IoSliceMut, os::fd::AsRawFd};
    const TAILLE: usize = cmsg_space::<[nix::libc::timespec; 3]>()
        + cmsg_space::<(nix::libc::sock_extended_err, nix::libc::sockaddr_in)>();
    #[repr(C)]
    struct Controle {
        alignement: [nix::libc::cmsghdr; 0],
        octets: [u8; TAILLE],
    }
    let mut controle = Controle {
        alignement: [],
        octets: [0; TAILLE],
    };
    let mut iov = [IoSliceMut::new(tampon)];
    let msg = recvmsg::<()>(
        socket.as_raw_fd(),
        &mut iov,
        Some(&mut controle.octets),
        flags,
    )?;
    let mut date = None;
    let mut id = None;
    for c in msg.cmsgs()? {
        match c {
            ControlMessageOwned::ScmTimestampsns(t) => {
                if let (Ok(sec), Ok(ns)) = (
                    u64::try_from(t.system.tv_sec()),
                    u32::try_from(t.system.tv_nsec()),
                ) {
                    date = SystemTime::UNIX_EPOCH.checked_add(Duration::new(sec, ns));
                }
            }
            ControlMessageOwned::Ipv4RecvErr(e, _)
                if e.ee_origin == nix::libc::SO_EE_ORIGIN_TIMESTAMPING =>
            {
                id = Some(e.ee_data)
            }
            _ => {}
        }
    }
    Ok((msg.bytes, date, id))
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Repere {
    civil: SystemTime,
    mono: Instant,
    incertitude: Duration,
}

#[cfg(target_os = "linux")]
impl Repere {
    fn lire() -> Self {
        let avant = Instant::now();
        let civil = SystemTime::now();
        let largeur = avant.elapsed();
        Self {
            civil,
            mono: avant + largeur / 2,
            incertitude: largeur / 2,
        }
    }
}

#[cfg(target_os = "linux")]
struct Correspondance {
    repere: Repere,
    debut: Instant,
}

#[cfg(target_os = "linux")]
impl Correspondance {
    fn new() -> Self {
        let repere = Repere::lire();
        Self {
            repere,
            debut: repere.mono,
        }
    }

    fn convertir(&mut self, date: SystemTime, nouveau: Repere) -> Option<Instant> {
        // REALTIME et MONOTONIC avancent ensemble sous Linux, sauf saut de
        // l'heure civile ou suspension. Fermer alors l'ancienne époque.
        let attendu = self
            .repere
            .civil
            .checked_add(nouveau.mono - self.repere.mono)?;
        let ecart = nouveau
            .civil
            .duration_since(attendu)
            .unwrap_or_else(|e| e.duration());
        let saut = ecart > self.repere.incertitude + nouveau.incertitude + Duration::from_micros(1);
        if saut {
            self.debut = nouveau.mono;
        }
        if saut || nouveau.incertitude <= self.repere.incertitude {
            self.repere = nouveau;
        }
        let instant = match date.duration_since(self.repere.civil) {
            Ok(d) => self.repere.mono.checked_add(d)?,
            Err(e) => self.repere.mono.checked_sub(e.duration())?,
        };
        (instant >= self.debut && instant <= nouveau.mono + nouveau.incertitude).then_some(instant)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn un_datagramme_en_attente_garde_l_instant_de_son_arrivee() {
        use std::net::Ipv4Addr;
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let emetteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let emission = Emission::activer(&socket);
        assert!(emission.au_noyau());
        let mut reception = Reception::new(&socket, true);
        emetteur
            .send_to(b"x", socket.local_addr().unwrap())
            .unwrap();
        // peek confirme la présence de ce paquet sans le consommer.
        let mut tampon = [0; 1];
        socket.peek(&mut tampon).unwrap();
        let en_file = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let (n, arrivee) = reception.recevoir(&mut tampon).unwrap();
        assert_eq!((n, tampon), (1, [b'x']));
        assert!(arrivee.unwrap() <= en_file);
    }

    #[test]
    fn un_recu_d_emission_tardif_ne_date_pas_le_datagramme_suivant() {
        use std::net::{Ipv4Addr, SocketAddr};
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(dest) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        let mut emission = Emission::activer(&socket);
        // Laisser le reçu n° 0 dans la file du noyau comme s’il était arrivé
        // après le repli de l’envoi précédent.
        socket.send_to(b"a", dest).unwrap();
        emission.noyau.as_mut().unwrap().1 = 1;
        std::thread::sleep(Duration::from_millis(10));
        let avant = Instant::now();
        let depart = emission.envoyer(&socket, b"b", dest).unwrap();
        assert!((avant..=Instant::now()).contains(&depart));
        let restant = lire_message(
            &socket,
            &mut [],
            nix::sys::socket::MsgFlags::MSG_ERRQUEUE | nix::sys::socket::MsgFlags::MSG_DONTWAIT,
        );
        assert_eq!(restant.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn un_saut_civil_invalide_les_paquets_de_l_epoque_precedente() {
        let debut = Instant::now();
        let civil = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let s = Duration::from_secs(1);
        for nouveau_civil in [civil - 10 * s, civil + 10 * s] {
            let ancien = Repere {
                civil,
                mono: debut,
                incertitude: Duration::ZERO,
            };
            let nouveau = Repere {
                civil: nouveau_civil,
                mono: debut + s,
                ..ancien
            };
            let mut correspondance = Correspondance {
                repere: ancien,
                debut,
            };
            assert_eq!(correspondance.convertir(civil + s / 2, nouveau), None);
            let suivant = Repere {
                civil: nouveau_civil + s,
                mono: debut + 2 * s,
                ..ancien
            };
            assert_eq!(
                correspondance.convertir(nouveau_civil + s / 2, suivant),
                Some(debut + s + s / 2)
            );
        }
    }
}
