//! Banc d'essai PTP, sans réseau ni droits : un MAÎTRE PTPv2 simulé, écrit
//! à la main (IEEE 1588-2008, deux temps : Sync puis Follow_Up, Announce,
//! réponse aux Delay_Req), sur 127.0.0.1 et des ports libres.
//!
//! `ptp4l` n'est pas installé sur Shrek (et il faudrait des droits pour les
//! ports 319/320) : ce maître le remplace. Il est volontairement INDÉPENDANT
//! de `statime` (aucun type de la bibliothèque, encodage octet par octet
//! d'après la norme) : si le suiveur se cale sur lui, c'est que `statime`
//! comprend un maître 1588-2008 qui n'est pas lui-même.
//!
//! Son horloge est une horloge « vraie » connue du banc : l'horloge
//! monotone, décalée de `decalage_ns` et dérivant de `derive_ppm`. Le banc
//! compare donc directement le temps du suiveur au temps du maître, au même
//! instant, dans le même processus.
//!
//! Jamais utilisé en production.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::horloge_virtuelle::HorlogeMonotone;

const SYNC: u8 = 0x0;
const DELAY_REQ: u8 = 0x1;
const FOLLOW_UP: u8 = 0x8;
const DELAY_RESP: u8 = 0x9;
const ANNOUNCE: u8 = 0xB;

/// Les réglages du maître simulé.
#[derive(Debug, Clone, Copy)]
pub struct ConfigMaitre {
    pub identite: [u8; 8],
    pub domaine: u8,
    pub priorite1: u8,
    /// Décalage initial de son horloge sur l'horloge monotone du banc.
    pub decalage_ns: i64,
    /// Dérive de son oscillateur par rapport à celle du banc.
    pub derive_ppm: f64,
    pub log_sync: i8,
    pub log_annonce: i8,
}

impl ConfigMaitre {
    /// Un maître typique : identité d'un appareil fictif, domaine 0,
    /// décalé d'environ 37 s (TAI) et 3 heures, +40 ppm, Sync à 8 Hz,
    /// Announce à 4 Hz (cadences accélérées pour le banc).
    pub fn par_defaut() -> Self {
        Self {
            identite: [0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56],
            domaine: 0,
            priorite1: 128,
            decalage_ns: (3 * 3600 + 37) * 1_000_000_000 + 123_456_789,
            derive_ppm: 40.0,
            log_sync: -3,
            log_annonce: -2,
        }
    }
}

/// L'horloge du maître : `monotone + décalage + dérive`.
#[derive(Debug, Clone, Copy)]
pub struct HorlogeDuMaitre {
    monotone: HorlogeMonotone,
    origine: u64,
    decalage_ns: i64,
    derive_ppm: f64,
}

impl HorlogeDuMaitre {
    pub fn new(decalage_ns: i64, derive_ppm: f64) -> Self {
        let monotone = HorlogeMonotone::new();
        Self {
            monotone,
            origine: monotone.ns(),
            decalage_ns,
            derive_ppm,
        }
    }

    pub fn maintenant_ns(&self) -> u64 {
        let m = self.monotone.ns();
        let ecoule = (m - self.origine) as f64;
        let v = i128::from(m)
            + i128::from(self.decalage_ns)
            + (ecoule * self.derive_ppm * 1e-6) as i128;
        u64::try_from(v.max(0)).unwrap_or(u64::MAX)
    }
}

fn duree_log2(log: i8) -> Duration {
    Duration::from_secs_f64(2f64.powi(i32::from(log)))
}

fn horodatage(ns: u64) -> [u8; 10] {
    let secondes = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    let mut t = [0u8; 10];
    t[..6].copy_from_slice(&secondes.to_be_bytes()[2..]);
    t[6..].copy_from_slice(&nanos.to_be_bytes());
    t
}

struct Entete {
    type_msg: u8,
    domaine: u8,
    drapeaux: [u8; 2],
    port: [u8; 10],
    sequence: u16,
    controle: u8,
    log_intervalle: i8,
}

fn message(e: &Entete, corps: &[u8]) -> Vec<u8> {
    let longueur = (34 + corps.len()) as u16;
    let mut m = Vec::with_capacity(usize::from(longueur));
    m.push(e.type_msg & 0x0f); // transportSpecific 0
    m.push(0x02); // versionPTP 2, minor 0 : IEEE 1588-2008
    m.extend_from_slice(&longueur.to_be_bytes());
    m.push(e.domaine);
    m.push(0);
    m.extend_from_slice(&e.drapeaux);
    m.extend_from_slice(&[0u8; 8]); // correctionField
    m.extend_from_slice(&[0u8; 4]);
    m.extend_from_slice(&e.port);
    m.extend_from_slice(&e.sequence.to_be_bytes());
    m.push(e.controle);
    m.push(e.log_intervalle as u8);
    m.extend_from_slice(corps);
    m
}

/// Ce que le maître a fait, pour les assertions.
#[derive(Debug, Default)]
pub struct CompteursMaitre {
    pub sync: AtomicU64,
    pub annonces: AtomicU64,
    pub delay_req: AtomicU64,
}

pub struct MaitreSimule {
    cfg: ConfigMaitre,
    horloge: HorlogeDuMaitre,
    evenement: UdpSocket,
    general: UdpSocket,
    destinations: Arc<Mutex<Option<(SocketAddrV4, SocketAddrV4)>>>,
    arret: Arc<AtomicBool>,
    fils: Mutex<Vec<JoinHandle<()>>>,
    pub compteurs: Arc<CompteursMaitre>,
}

fn v4(s: &UdpSocket) -> io::Result<SocketAddrV4> {
    match s.local_addr()? {
        std::net::SocketAddr::V4(a) => Ok(a),
        std::net::SocketAddr::V6(_) => Err(io::Error::other("IPv6")),
    }
}

impl MaitreSimule {
    /// Lie ses deux sockets sur 127.0.0.1 (ports libres) et lance ses fils.
    /// Il n'émet rien tant qu'on ne lui a pas dit où ([`Self::servir`]).
    pub fn demarrer(cfg: ConfigMaitre) -> io::Result<Self> {
        let libre = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        Self::demarrer_sur(cfg, libre, libre)
    }

    /// Comme [`Self::demarrer`], sur des adresses données : un maître qui
    /// en remplace un autre, au même endroit du réseau.
    pub fn demarrer_sur(
        cfg: ConfigMaitre,
        evenement: SocketAddrV4,
        general: SocketAddrV4,
    ) -> io::Result<Self> {
        let evenement = UdpSocket::bind(evenement)?;
        let general = UdpSocket::bind(general)?;
        evenement.set_read_timeout(Some(Duration::from_millis(50)))?;
        let m = Self {
            cfg,
            horloge: HorlogeDuMaitre::new(cfg.decalage_ns, cfg.derive_ppm),
            evenement,
            general,
            destinations: Arc::new(Mutex::new(None)),
            arret: Arc::new(AtomicBool::new(false)),
            fils: Mutex::new(Vec::new()),
            compteurs: Arc::new(CompteursMaitre::default()),
        };
        m.lancer()?;
        Ok(m)
    }

    /// Les adresses (événement, général) du maître.
    pub fn adresses(&self) -> (SocketAddrV4, SocketAddrV4) {
        (
            v4(&self.evenement).expect("adresse"),
            v4(&self.general).expect("adresse"),
        )
    }

    /// Commence à émettre vers ce suiveur (événement, général).
    pub fn servir(&self, suiveur: (SocketAddrV4, SocketAddrV4)) {
        *self.destinations.lock().unwrap() = Some(suiveur);
    }

    /// Le temps vrai du maître.
    pub fn maintenant_ns(&self) -> u64 {
        self.horloge.maintenant_ns()
    }

    pub fn identite(&self) -> [u8; 8] {
        self.cfg.identite
    }

    pub fn arreter(&self) {
        self.arret.store(true, Ordering::Release);
        for f in self.fils.lock().unwrap().drain(..) {
            let _ = f.join();
        }
    }

    fn port(&self) -> [u8; 10] {
        let mut p = [0u8; 10];
        p[..8].copy_from_slice(&self.cfg.identite);
        p[9] = 1;
        p
    }

    fn lancer(&self) -> io::Result<()> {
        let cfg = self.cfg;
        let port = self.port();
        let horloge = self.horloge;
        let mut fils = self.fils.lock().unwrap();

        // L'émetteur : Sync + Follow_Up, Announce.
        {
            let (ev, ge) = (self.evenement.try_clone()?, self.general.try_clone()?);
            let destinations = self.destinations.clone();
            let arret = self.arret.clone();
            let compteurs = self.compteurs.clone();
            fils.push(
                std::thread::Builder::new()
                    .name("banc-ptp-maitre".into())
                    .spawn(move || {
                        let pas_sync = duree_log2(cfg.log_sync);
                        let pas_annonce = duree_log2(cfg.log_annonce);
                        let debut = std::time::Instant::now();
                        let (mut seq_sync, mut seq_annonce) = (0u16, 0u16);
                        let mut prochaine_sync = debut;
                        let mut prochaine_annonce = debut;
                        while !arret.load(Ordering::Acquire) {
                            let maintenant = std::time::Instant::now();
                            let Some((d_ev, d_ge)) = *destinations.lock().unwrap() else {
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            };
                            if maintenant >= prochaine_annonce {
                                let mut corps = Vec::with_capacity(30);
                                corps.extend_from_slice(&horodatage(horloge.maintenant_ns()));
                                corps.extend_from_slice(&37i16.to_be_bytes());
                                corps.push(0);
                                corps.push(cfg.priorite1);
                                corps.extend_from_slice(&[248, 0xFE, 0xFF, 0xFF]);
                                corps.push(128);
                                corps.extend_from_slice(&cfg.identite);
                                corps.extend_from_slice(&0u16.to_be_bytes());
                                corps.push(0xA0); // INTERNAL_OSCILLATOR
                                let m = message(
                                    &Entete {
                                        type_msg: ANNOUNCE,
                                        domaine: cfg.domaine,
                                        drapeaux: [0, 0x0C],
                                        port,
                                        sequence: seq_annonce,
                                        controle: 5,
                                        log_intervalle: cfg.log_annonce,
                                    },
                                    &corps,
                                );
                                let _ = ge.send_to(&m, d_ge);
                                seq_annonce = seq_annonce.wrapping_add(1);
                                compteurs.annonces.fetch_add(1, Ordering::Relaxed);
                                prochaine_annonce += pas_annonce;
                            }
                            if maintenant >= prochaine_sync {
                                let sync = message(
                                    &Entete {
                                        type_msg: SYNC,
                                        domaine: cfg.domaine,
                                        drapeaux: [0x02, 0x0C], // twoStepFlag
                                        port,
                                        sequence: seq_sync,
                                        controle: 0,
                                        log_intervalle: cfg.log_sync,
                                    },
                                    &[0u8; 10],
                                );
                                if ev.send_to(&sync, d_ev).is_ok() {
                                    // Horodatage logiciel, comme le suiveur.
                                    let t1 = horloge.maintenant_ns();
                                    let suivi = message(
                                        &Entete {
                                            type_msg: FOLLOW_UP,
                                            domaine: cfg.domaine,
                                            drapeaux: [0, 0x0C],
                                            port,
                                            sequence: seq_sync,
                                            controle: 2,
                                            log_intervalle: cfg.log_sync,
                                        },
                                        &horodatage(t1),
                                    );
                                    let _ = ge.send_to(&suivi, d_ge);
                                    compteurs.sync.fetch_add(1, Ordering::Relaxed);
                                }
                                seq_sync = seq_sync.wrapping_add(1);
                                prochaine_sync += pas_sync;
                            }
                            let suivante = prochaine_sync.min(prochaine_annonce);
                            std::thread::sleep(
                                suivante
                                    .saturating_duration_since(std::time::Instant::now())
                                    .min(Duration::from_millis(20)),
                            );
                        }
                    })?,
            );
        }

        // Le répondeur : Delay_Req → Delay_Resp.
        {
            let (ev, ge) = (self.evenement.try_clone()?, self.general.try_clone()?);
            let destinations = self.destinations.clone();
            let arret = self.arret.clone();
            let compteurs = self.compteurs.clone();
            fils.push(
                std::thread::Builder::new()
                    .name("banc-ptp-reponse".into())
                    .spawn(move || {
                        let mut t = [0u8; 1500];
                        while !arret.load(Ordering::Acquire) {
                            let Ok(n) = ev.recv(&mut t) else { continue };
                            let t4 = horloge.maintenant_ns();
                            let r = &t[..n];
                            if n < 44 || r[0] & 0x0f != DELAY_REQ || r[4] != cfg.domaine {
                                continue;
                            }
                            let Some((_, d_ge)) = *destinations.lock().unwrap() else {
                                continue;
                            };
                            compteurs.delay_req.fetch_add(1, Ordering::Relaxed);
                            let mut corps = Vec::with_capacity(20);
                            corps.extend_from_slice(&horodatage(t4));
                            corps.extend_from_slice(&r[20..30]); // requestingPortIdentity
                            let reponse = message(
                                &Entete {
                                    type_msg: DELAY_RESP,
                                    domaine: cfg.domaine,
                                    drapeaux: [0, 0x0C],
                                    port,
                                    sequence: u16::from_be_bytes([r[30], r[31]]),
                                    controle: 3,
                                    log_intervalle: 0,
                                },
                                &corps,
                            );
                            let _ = ge.send_to(&reponse, d_ge);
                        }
                    })?,
            );
        }
        Ok(())
    }
}

impl Drop for MaitreSimule {
    fn drop(&mut self) {
        self.arreter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_horodatage_ptp_est_sur_48_bits_de_secondes_et_32_de_nanosecondes() {
        // 0x1_2345_6789 s : le plus grand temps qu'un u64 de ns sache porter
        // tient en 5 octets ; le sixième (poids fort) reste nul.
        let t = horodatage(0x0001_2345_6789 * 1_000_000_000 + 999_999_999);
        assert_eq!(&t[..6], &[0x00, 0x01, 0x23, 0x45, 0x67, 0x89]);
        assert_eq!(&t[6..], &999_999_999u32.to_be_bytes());
    }

    #[test]
    fn l_en_tete_suit_ieee_1588_2008() {
        let m = message(
            &Entete {
                type_msg: SYNC,
                domaine: 0,
                drapeaux: [0x02, 0],
                port: [1, 2, 3, 4, 5, 6, 7, 8, 0, 1],
                sequence: 0x0102,
                controle: 0,
                log_intervalle: -3,
            },
            &[0u8; 10],
        );
        assert_eq!(m.len(), 44);
        assert_eq!(m[0], 0x00);
        assert_eq!(m[1], 0x02, "PTPv2.0");
        assert_eq!(&m[2..4], &44u16.to_be_bytes());
        assert_eq!(m[6], 0x02, "deux temps");
        assert_eq!(&m[20..30], &[1, 2, 3, 4, 5, 6, 7, 8, 0, 1]);
        assert_eq!(&m[30..32], &[1, 2]);
        assert_eq!(m[33], 0xFD, "log -3");
    }

    #[test]
    fn l_horloge_du_maitre_derive_comme_annonce() {
        let h = HorlogeDuMaitre::new(5_000_000_000, 1000.0);
        let m = HorlogeMonotone::new();
        let (a, ma) = (h.maintenant_ns(), m.ns());
        std::thread::sleep(Duration::from_millis(200));
        let (b, mb) = (h.maintenant_ns(), m.ns());
        let ppm = ((b - a) as f64 / (mb - ma) as f64 - 1.0) * 1e6;
        assert!((ppm - 1000.0).abs() < 50.0, "{ppm}");
        assert!(a.abs_diff(ma + 5_000_000_000) < 10_000_000);
    }
}
