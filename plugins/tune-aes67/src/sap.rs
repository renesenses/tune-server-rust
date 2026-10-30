//! L'annonce SAP (RFC 2974) d'un flux AES67.
//!
//! Les logiciels de contrôle des récepteurs AES67 découvrent les flux par SAP (§ 1.2 de l'étude). Le
//! paquet : un en-tête de 8 octets (IPv4, sans authentification ni
//! chiffrement), le type de charge `application/sdp` terminé par un octet nul,
//! puis le SDP. L'annonce est répétée ; un paquet de retrait (bit T) part à
//! l'arrêt.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::{debug, warn};

/// Groupe et port SAP de la portée administrative globale (239.255.255.255),
/// ceux qu'écoutent les récepteurs AES67 et leurs logiciels de contrôle.
pub const DESTINATION_SAP: SocketAddrV4 =
    SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 255), 9875);

/// Intervalle d'annonce. La RFC 2974 le calcule sur la bande passante SAP
/// d'une portée ; l'usage des émetteurs AES67 est de l'ordre de 30 s.
pub const INTERVALLE_SAP: Duration = Duration::from_secs(30);

const TYPE_DE_CHARGE: &[u8] = b"application/sdp\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeSap {
    Annonce,
    Retrait,
}

/// Le hachage d'identifiant de message (16 bits) : il doit changer quand le
/// SDP change, et ne jamais valoir zéro (zéro désactive le contrôle chez le
/// récepteur). FNV-1a replié sur 16 bits.
pub fn hachage_du_message(sdp: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for o in sdp.as_bytes() {
        h ^= u32::from(*o);
        h = h.wrapping_mul(0x0100_0193);
    }
    let replie = ((h >> 16) ^ (h & 0xffff)) as u16;
    if replie == 0 { 1 } else { replie }
}

/// Un paquet SAP complet.
pub fn paquet(type_sap: TypeSap, hachage: u16, source: Ipv4Addr, sdp: &str) -> Vec<u8> {
    // V=1 (001), A=0 (IPv4), R=0, T, E=0, C=0.
    let premier = match type_sap {
        TypeSap::Annonce => 0x20,
        TypeSap::Retrait => 0x24,
    };
    let mut p = Vec::with_capacity(8 + TYPE_DE_CHARGE.len() + sdp.len());
    p.push(premier);
    p.push(0); // pas d'authentification
    p.extend_from_slice(&hachage.to_be_bytes());
    p.extend_from_slice(&source.octets());
    p.extend_from_slice(TYPE_DE_CHARGE);
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// Ce que le récepteur de test relit d'un paquet SAP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SapLu {
    pub type_sap: TypeSap,
    pub hachage: u16,
    pub source: Ipv4Addr,
    pub sdp: String,
}

pub fn lire(paquet: &[u8]) -> Result<SapLu, String> {
    if paquet.len() < 8 {
        return Err("paquet SAP trop court".into());
    }
    let premier = paquet[0];
    if premier >> 5 != 1 {
        return Err(format!("version SAP {}, attendu 1", premier >> 5));
    }
    if premier & 0x10 != 0 {
        return Err("SAP IPv6 non pris en charge".into());
    }
    if premier & 0x03 != 0 {
        return Err("SAP chiffré ou compressé".into());
    }
    let type_sap = if premier & 0x04 != 0 {
        TypeSap::Retrait
    } else {
        TypeSap::Annonce
    };
    let auth = usize::from(paquet[1]) * 4;
    let hachage = u16::from_be_bytes([paquet[2], paquet[3]]);
    let source = Ipv4Addr::new(paquet[4], paquet[5], paquet[6], paquet[7]);
    let reste = paquet.get(8 + auth..).ok_or("authentification tronquée")?;
    // Le type de charge est facultatif : un SDP commence par « v=0 ».
    let sdp = if reste.starts_with(b"v=0") {
        reste
    } else {
        let nul = reste
            .iter()
            .position(|&o| o == 0)
            .ok_or("type de charge sans octet nul")?;
        if &reste[..nul] != b"application/sdp" {
            return Err("charge SAP qui n'est pas un SDP".into());
        }
        &reste[nul + 1..]
    };
    Ok(SapLu {
        type_sap,
        hachage,
        source,
        sdp: String::from_utf8(sdp.to_vec()).map_err(|e| format!("SDP non UTF-8 : {e}"))?,
    })
}

/// L'annonceur d'un flux : un fil qui répète l'annonce et envoie le retrait
/// quand on l'arrête (ou qu'on le lâche).
pub struct AnnonceurSap {
    arret: Arc<AtomicBool>,
    fil: Option<JoinHandle<()>>,
}

impl AnnonceurSap {
    /// `socket` est déjà réglé (interface, TTL, DSCP) par l'appelant.
    pub fn demarrer(
        socket: UdpSocket,
        destination: SocketAddrV4,
        source: Ipv4Addr,
        sdp: String,
        intervalle: Duration,
    ) -> Self {
        let arret = Arc::new(AtomicBool::new(false));
        let drapeau = arret.clone();
        let fil = std::thread::Builder::new()
            .name("aes67-sap".into())
            .spawn(move || {
                let hachage = hachage_du_message(&sdp);
                let annonce = paquet(TypeSap::Annonce, hachage, source, &sdp);
                let pas = Duration::from_millis(50);
                'annonces: loop {
                    if let Err(e) = socket.send_to(&annonce, destination) {
                        warn!(error = %e, %destination, "aes67_sap_envoi_refuse");
                    } else {
                        debug!(%destination, "aes67_sap_annonce");
                    }
                    let mut attendu = Duration::ZERO;
                    while attendu < intervalle {
                        if drapeau.load(Ordering::Acquire) {
                            break 'annonces;
                        }
                        std::thread::sleep(pas);
                        attendu += pas;
                    }
                }
                let retrait = paquet(TypeSap::Retrait, hachage, source, &sdp);
                let _ = socket.send_to(&retrait, destination);
                debug!(%destination, "aes67_sap_retrait");
            })
            .ok();
        Self { arret, fil }
    }

    /// Arrête les annonces et envoie le retrait. Idempotent.
    pub fn arreter(&mut self) {
        self.arret.store(true, Ordering::Release);
        if let Some(fil) = self.fil.take() {
            let _ = fil.join();
        }
    }
}

impl Drop for AnnonceurSap {
    fn drop(&mut self) {
        self.arreter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\ns=Tune - Salon\r\nm=audio 5004 RTP/AVP 98\r\n";

    #[test]
    fn l_en_tete_sap_suit_la_rfc_2974() {
        let p = paquet(TypeSap::Annonce, 0xabcd, Ipv4Addr::new(10, 0, 0, 2), SDP);
        assert_eq!(&p[..8], &[0x20, 0, 0xab, 0xcd, 10, 0, 0, 2]);
        assert_eq!(&p[8..24], b"application/sdp\0");
        assert_eq!(&p[24..], SDP.as_bytes());
        let r = paquet(TypeSap::Retrait, 0xabcd, Ipv4Addr::new(10, 0, 0, 2), SDP);
        assert_eq!(r[0], 0x24, "bit T du retrait");
    }

    #[test]
    fn le_paquet_se_relit() {
        let p = paquet(TypeSap::Retrait, 7, Ipv4Addr::LOCALHOST, SDP);
        let lu = lire(&p).unwrap();
        assert_eq!(lu.type_sap, TypeSap::Retrait);
        assert_eq!(lu.hachage, 7);
        assert_eq!(lu.source, Ipv4Addr::LOCALHOST);
        assert_eq!(lu.sdp, SDP);
        // Sans type de charge (permis par la RFC) : le SDP suit l'en-tête.
        let mut nu = p[..8].to_vec();
        nu.extend_from_slice(SDP.as_bytes());
        assert_eq!(lire(&nu).unwrap().sdp, SDP);
        assert!(lire(&p[..5]).is_err());
    }

    #[test]
    fn le_hachage_suit_le_sdp_et_ne_vaut_jamais_zero() {
        let a = hachage_du_message(SDP);
        let b = hachage_du_message(&SDP.replace("Salon", "Bureau"));
        assert_ne!(a, 0);
        assert_ne!(a, b);
        assert_eq!(a, hachage_du_message(SDP), "stable");
    }

    #[test]
    fn l_annonceur_annonce_puis_retire_sur_la_boucle_locale() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        ecoute
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let std::net::SocketAddr::V4(destination) = ecoute.local_addr().unwrap() else {
            panic!("IPv4 attendu");
        };
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut annonceur = AnnonceurSap::demarrer(
            socket,
            destination,
            Ipv4Addr::LOCALHOST,
            SDP.into(),
            Duration::from_millis(100),
        );
        let mut tampon = [0u8; 2048];
        let n = ecoute.recv(&mut tampon).unwrap();
        assert_eq!(lire(&tampon[..n]).unwrap().type_sap, TypeSap::Annonce);
        let n = ecoute.recv(&mut tampon).unwrap();
        assert_eq!(
            lire(&tampon[..n]).unwrap().type_sap,
            TypeSap::Annonce,
            "annonce répétée"
        );
        annonceur.arreter();
        // Le dernier paquet reçu est le retrait.
        let mut dernier = None;
        ecoute
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        while let Ok(n) = ecoute.recv(&mut tampon) {
            dernier = Some(lire(&tampon[..n]).unwrap().type_sap);
        }
        assert_eq!(dernier, Some(TypeSap::Retrait));
    }
}
