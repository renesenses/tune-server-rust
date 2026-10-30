//! Banc d'essai (étape E0) : un récepteur de test, sans carte son.
//!
//! RÈGLE : aucun son audible pendant les essais. Le récepteur ne joue rien :
//! il garde les datagrammes, vérifie les en-têtes, reconstitue le PCM, en
//! calcule l'empreinte (SHA-256) et peut tout écrire dans un fichier pcap
//! (en-têtes IPv4/UDP reconstitués, `LINKTYPE_RAW`) lisible par Wireshark ou
//! `tshark`, et le PCM brut dans un fichier.
//!
//! Le pcap est écrit PAR LE RÉCEPTEUR à partir de ce qu'il a reçu : ce n'est
//! pas une capture de l'interface (tcpdump demande des droits que le banc n'a
//! pas). Il prouve le contenu et la cadence d'arrivée, pas le DSCP ni le TTL
//! posés sur le fil.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::net::{SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::paquetisation::Paquetiseur;
use crate::producteur::{self, LecteurPcm, SourceAudio};
use crate::rtp::EnteteRtp;

/// Un datagramme reçu, avec ses deux instants d'arrivée : monotone (pour la
/// gigue) et UNIX (pour le pcap).
#[derive(Debug, Clone)]
pub struct DatagrammeRecu {
    pub arrivee: Instant,
    pub arrivee_unix: Duration,
    pub source: SocketAddrV4,
    pub donnees: Vec<u8>,
}

pub struct RecepteurDeTest {
    adresse: SocketAddrV4,
    arret: Arc<AtomicBool>,
    fil: JoinHandle<Vec<DatagrammeRecu>>,
}

impl RecepteurDeTest {
    /// Écoute en unicast sur `adresse` (port 0 : un port libre).
    pub fn ecouter(adresse: SocketAddrV4) -> io::Result<Self> {
        let socket = UdpSocket::bind(adresse)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        let std::net::SocketAddr::V4(adresse) = socket.local_addr()? else {
            return Err(io::Error::other("IPv4 attendu"));
        };
        let arret = Arc::new(AtomicBool::new(false));
        let drapeau = arret.clone();
        let fil = std::thread::Builder::new()
            .name("aes67-banc".into())
            .spawn(move || {
                let mut recus = Vec::new();
                let mut tampon = vec![0u8; 65_536];
                while !drapeau.load(Ordering::Acquire) {
                    match socket.recv_from(&mut tampon) {
                        Ok((n, std::net::SocketAddr::V4(source))) => recus.push(DatagrammeRecu {
                            arrivee: Instant::now(),
                            arrivee_unix: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default(),
                            source,
                            donnees: tampon[..n].to_vec(),
                        }),
                        Ok(_) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
                recus
            })?;
        Ok(Self {
            adresse,
            arret,
            fil,
        })
    }

    pub fn adresse(&self) -> SocketAddrV4 {
        self.adresse
    }

    /// Arrête l'écoute et rend la capture.
    pub fn terminer(self) -> Capture {
        self.arret.store(true, Ordering::Release);
        Capture {
            destination: self.adresse,
            datagrammes: self.fil.join().unwrap_or_default(),
        }
    }
}

pub struct Capture {
    pub destination: SocketAddrV4,
    pub datagrammes: Vec<DatagrammeRecu>,
}

/// Ce que le banc constate sur un flux RTP.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Analyse {
    pub paquets: usize,
    pub erreurs_d_en_tete: usize,
    pub types_de_charge: BTreeSet<u8>,
    pub ssrc: BTreeSet<u32>,
    pub tailles_de_charge: BTreeSet<usize>,
    /// Numéros de séquence manquants (pertes) ou en désordre.
    pub pertes: u64,
    pub desordres: u64,
    /// Paquets dont l'horodatage n'avance pas d'exactement une durée de
    /// paquet (hors paquets marqués, qui ouvrent une nouvelle origine).
    pub sauts_d_horodatage: u64,
    pub marqueurs: usize,
    pub octets_pcm: usize,
    pub sha256_pcm: String,
    /// Intervalle moyen d'arrivée, et écart maximal à l'intervalle nominal.
    pub intervalle_moyen_us: f64,
    pub gigue_max_us: f64,
}

impl Capture {
    /// Les paquets RTP lisibles, dans l'ordre d'arrivée.
    fn paquets(&self) -> Vec<(&DatagrammeRecu, EnteteRtp, &[u8])> {
        self.datagrammes
            .iter()
            .filter_map(|d| EnteteRtp::lire(&d.donnees).ok().map(|(e, c)| (d, e, c)))
            .collect()
    }

    /// Le PCM L24 reçu, charges mises bout à bout dans l'ordre d'arrivée.
    pub fn pcm(&self) -> Vec<u8> {
        self.paquets()
            .iter()
            .flat_map(|(_, _, c)| c.iter().copied())
            .collect()
    }

    pub fn analyser(&self, trames_par_paquet: u32, duree_paquet: Duration) -> Analyse {
        let paquets = self.paquets();
        let mut a = Analyse {
            paquets: paquets.len(),
            erreurs_d_en_tete: self.datagrammes.len() - paquets.len(),
            types_de_charge: BTreeSet::new(),
            ssrc: BTreeSet::new(),
            tailles_de_charge: BTreeSet::new(),
            pertes: 0,
            desordres: 0,
            sauts_d_horodatage: 0,
            marqueurs: 0,
            octets_pcm: 0,
            sha256_pcm: hex(&Sha256::digest(self.pcm())),
            intervalle_moyen_us: 0.0,
            gigue_max_us: 0.0,
        };
        let nominal_us = duree_paquet.as_secs_f64() * 1e6;
        let mut somme_us = 0.0;
        for (i, (d, e, c)) in paquets.iter().enumerate() {
            a.types_de_charge.insert(e.type_charge);
            a.ssrc.insert(e.ssrc);
            a.tailles_de_charge.insert(c.len());
            a.octets_pcm += c.len();
            if e.marqueur {
                a.marqueurs += 1;
            }
            if i == 0 {
                continue;
            }
            let (dp, ep, _) = &paquets[i - 1];
            let ecart = e.sequence.wrapping_sub(ep.sequence);
            if ecart == 0 || ecart > 0x8000 {
                a.desordres += 1;
            } else if ecart > 1 {
                a.pertes += u64::from(ecart - 1);
            }
            if !e.marqueur && e.horodatage.wrapping_sub(ep.horodatage) != trames_par_paquet {
                a.sauts_d_horodatage += 1;
            }
            if !e.marqueur {
                let intervalle_us = d.arrivee.duration_since(dp.arrivee).as_secs_f64() * 1e6;
                somme_us += intervalle_us;
                a.gigue_max_us = a.gigue_max_us.max((intervalle_us - nominal_us).abs());
            }
        }
        let comptes = paquets
            .iter()
            .skip(1)
            .filter(|(_, e, _)| !e.marqueur)
            .count();
        if comptes > 0 {
            a.intervalle_moyen_us = somme_us / comptes as f64;
        }
        a
    }

    /// Écrit la capture au format pcap (LINKTYPE_RAW : IPv4 sans couche 2).
    pub fn ecrire_pcap(&self, chemin: &std::path::Path) -> io::Result<()> {
        let mut f = io::BufWriter::new(std::fs::File::create(chemin)?);
        // En-tête global : magie µs, version 2.4, fuseau 0, précision 0,
        // longueur de capture 65535, type de lien 101 (RAW).
        f.write_all(&0xa1b2_c3d4u32.to_le_bytes())?;
        f.write_all(&2u16.to_le_bytes())?;
        f.write_all(&4u16.to_le_bytes())?;
        f.write_all(&0i32.to_le_bytes())?;
        f.write_all(&0u32.to_le_bytes())?;
        f.write_all(&65_535u32.to_le_bytes())?;
        f.write_all(&101u32.to_le_bytes())?;
        for d in &self.datagrammes {
            let trame = ipv4_udp(d.source, self.destination, &d.donnees);
            f.write_all(&(d.arrivee_unix.as_secs() as u32).to_le_bytes())?;
            f.write_all(&d.arrivee_unix.subsec_micros().to_le_bytes())?;
            f.write_all(&(trame.len() as u32).to_le_bytes())?;
            f.write_all(&(trame.len() as u32).to_le_bytes())?;
            f.write_all(&trame)?;
        }
        f.flush()
    }
}

/// Un datagramme reconstitué en paquet IPv4/UDP (somme UDP à zéro, permise
/// en IPv4 ; somme IP calculée).
fn ipv4_udp(source: SocketAddrV4, destination: SocketAddrV4, charge: &[u8]) -> Vec<u8> {
    let longueur_udp = 8 + charge.len();
    let longueur_ip = 20 + longueur_udp;
    let mut p = Vec::with_capacity(longueur_ip);
    p.extend_from_slice(&[0x45, 0]);
    p.extend_from_slice(&(longueur_ip as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
    p.extend_from_slice(&source.ip().octets());
    p.extend_from_slice(&destination.ip().octets());
    let mut somme: u32 = p
        .chunks_exact(2)
        .map(|m| u32::from(u16::from_be_bytes([m[0], m[1]])))
        .sum();
    while somme > 0xffff {
        somme = (somme & 0xffff) + (somme >> 16);
    }
    p[10..12].copy_from_slice(&(!(somme as u16)).to_be_bytes());
    p.extend_from_slice(&source.port().to_be_bytes());
    p.extend_from_slice(&destination.port().to_be_bytes());
    p.extend_from_slice(&(longueur_udp as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(charge);
    p
}

pub fn hex(octets: &[u8]) -> String {
    octets.iter().map(|o| format!("{o:02x}")).collect()
}

/// Le PCM L24 qu'un flux de ce fichier DOIT transporter : le même décodeur,
/// le même rééchantillonnage, le même découpage, sans réseau. Le comparer à
/// [`Capture::pcm`] prouve que le transport ne perd, ne duplique ni ne
/// déplace aucun échantillon.
///
/// À appeler depuis un contexte tokio (le décodeur tourne sur un fil
/// bloquant du runtime) ; la lecture se fait sur un fil système à part.
pub fn pcm_attendu(
    chemin: &str,
    cadence: u32,
    canaux: u16,
    trames_par_paquet: usize,
) -> Result<Vec<u8>, String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "pcm_attendu demande un runtime tokio".to_string())?;
    let source = SourceAudio::Fichier(chemin.to_string());
    std::thread::spawn(move || {
        let (rx, tache) = producteur::lancer_le_decodeur(&runtime, source, cadence, canaux);
        let mut lecteur = LecteurPcm::new(rx, cadence, canaux);
        let mut p = Paquetiseur::new(usize::from(canaux), trames_par_paquet);
        let mut pcm = Vec::new();
        let lu = (|| -> Result<(), String> {
            while let Some(bloc) = lecteur.suivant()? {
                for c in p.pousser(&bloc) {
                    pcm.extend_from_slice(&c);
                }
            }
            Ok(())
        })();
        lecteur.fermer();
        producteur::attendre_le_decodeur(&runtime, tache)?;
        lu?;
        if let Some(c) = p.vider() {
            pcm.extend_from_slice(&c);
        }
        Ok(pcm)
    })
    .join()
    .map_err(|_| "le fil de lecture a paniqué".to_string())?
}

/// Écrit un fichier WAV PCM 16 bits : une sinusoïde par canal (fréquences
/// données), à -6 dBFS. Sert de piste de test.
pub fn ecrire_wav_sinus(
    chemin: &std::path::Path,
    cadence: u32,
    frequences: &[f64],
    duree: Duration,
) -> io::Result<()> {
    let canaux = frequences.len() as u16;
    let trames = (duree.as_secs_f64() * f64::from(cadence)).round() as u32;
    let octets = trames * u32::from(canaux) * 2;
    let mut f = io::BufWriter::new(std::fs::File::create(chemin)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + octets).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&canaux.to_le_bytes())?;
    f.write_all(&cadence.to_le_bytes())?;
    f.write_all(&(cadence * u32::from(canaux) * 2).to_le_bytes())?;
    f.write_all(&(canaux * 2).to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&octets.to_le_bytes())?;
    for n in 0..trames {
        for &fr in frequences {
            let t = f64::from(n) / f64::from(cadence);
            let v = (0.5 * (2.0 * std::f64::consts::PI * fr * t).sin() * 32767.0).round() as i16;
            f.write_all(&v.to_le_bytes())?;
        }
    }
    f.flush()
}

/// Puissance relative d'une fréquence dans un canal (Goertzel), pour vérifier
/// qu'un 1 kHz rééchantillonné reste un 1 kHz.
pub fn puissance_a(echantillons: &[f64], cadence: u32, frequence: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * frequence / f64::from(cadence);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in echantillons {
        let s = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (echantillons.len().max(1) as f64).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_somme_ip_reconstituee_est_valide() {
        let p = ipv4_udp(
            "127.0.0.1:40000".parse().unwrap(),
            "127.0.0.1:5004".parse().unwrap(),
            &[1, 2, 3],
        );
        assert_eq!(p.len(), 20 + 8 + 3);
        let mut somme: u32 = p[..20]
            .chunks_exact(2)
            .map(|m| u32::from(u16::from_be_bytes([m[0], m[1]])))
            .sum();
        while somme > 0xffff {
            somme = (somme & 0xffff) + (somme >> 16);
        }
        assert_eq!(somme, 0xffff, "une somme IP juste se replie en 0xffff");
    }

    #[test]
    fn goertzel_distingue_1000_de_1088_hz() {
        let s: Vec<f64> = (0..48_000)
            .map(|n| (2.0 * std::f64::consts::PI * 1000.0 * f64::from(n) / 48_000.0).sin())
            .collect();
        let a = puissance_a(&s, 48_000, 1000.0);
        let b = puissance_a(&s, 48_000, 1088.4);
        assert!(a > 1000.0 * b, "{a} contre {b}");
    }
}
