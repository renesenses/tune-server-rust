//! L'écoute passive des annonces `Da`.
//!
//! Les amplis diffusent leur état complet toutes les 200 ms sur le port
//! 45454. Écouter n'envoie rien à personne : c'est la seule partie du
//! protocole qui ne touche pas l'ampli. L'adresse de l'ampli est l'adresse
//! source du datagramme ; son identité stable est sa MAC. Un ampli qui ne
//! s'annonce plus depuis 2 s est considéré comme parti (débranché, réseau
//! coupé) ; en veille, il continue d'annoncer avec le bit « allumé » à 0.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::protocole::{Annonce, TAILLE_ANNONCE, analyser_annonce};

/// Silence au-delà duquel un ampli est considéré comme parti.
pub const EXPIRATION: Duration = Duration::from_secs(2);

/// Ce que l'on sait d'un ampli : sa dernière annonce valide.
#[derive(Debug, Clone)]
pub struct AmpliVu {
    pub ip: IpAddr,
    pub annonce: Annonce,
    pub vu_a: Instant,
}

impl AmpliVu {
    #[must_use]
    pub fn present(&self) -> bool {
        self.vu_a.elapsed() < EXPIRATION
    }
}

/// L'état des amplis entendus, par MAC.
#[derive(Default)]
pub struct Ecoute {
    amplis: Mutex<HashMap<String, AmpliVu>>,
    tache: Mutex<Option<JoinHandle<()>>>,
}

impl Ecoute {
    #[must_use]
    pub fn nouvelle() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn amplis(&self) -> std::sync::MutexGuard<'_, HashMap<String, AmpliVu>> {
        self.amplis
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Range un datagramme reçu de `ip`. Rend la MAC si c'était une annonce
    /// valide ; tout le reste (autre protocole, CRC faux) est ignoré.
    pub fn noter(&self, ip: IpAddr, octets: &[u8]) -> Option<String> {
        let annonce = match analyser_annonce(octets) {
            Ok(a) => a,
            Err(e) => {
                if octets.len() == TAILLE_ANNONCE {
                    debug!(%ip, erreur = %e, "devialet_air_annonce_refusee");
                }
                return None;
            }
        };
        let mac = annonce.mac_texte();
        let nouveau = {
            let mut amplis = self.amplis();
            let nouveau = amplis.get(&mac).is_none_or(|a| !a.present());
            amplis.insert(
                mac.clone(),
                AmpliVu {
                    ip,
                    annonce,
                    vu_a: Instant::now(),
                },
            );
            nouveau
        };
        if nouveau {
            info!(%ip, %mac, "devialet_air_ampli_entendu");
        }
        Some(mac)
    }

    /// Les amplis entendus il y a moins de 2 s.
    #[must_use]
    pub fn presents(&self) -> Vec<(String, AmpliVu)> {
        let mut v: Vec<_> = self
            .amplis()
            .iter()
            .filter(|(_, a)| a.present())
            .map(|(m, a)| (m.clone(), a.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// La dernière annonce d'un ampli, s'il est encore présent.
    #[must_use]
    pub fn ampli(&self, mac: &str) -> Option<AmpliVu> {
        self.amplis().get(mac).filter(|a| a.present()).cloned()
    }

    #[must_use]
    pub fn en_ecoute(&self) -> bool {
        self.tache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// Ouvre l'écoute sur `port` (45454 en service), si elle ne tourne pas
    /// déjà. SO_REUSEADDR et SO_REUSEPORT : un Roon sur la même machine garde
    /// ce port ouvert en permanence.
    pub fn demarrer(self: &Arc<Self>, port: u16) -> Result<(), String> {
        let mut tache = self
            .tache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tache.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(());
        }
        let socket = ouvrir_socket_d_ecoute(port)?;
        info!(port, "devialet_air_ecoute_ouverte");
        let moi = Arc::clone(self);
        *tache = Some(tokio::spawn(async move {
            let mut tampon = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut tampon).await {
                    Ok((n, source)) => {
                        moi.noter(source.ip(), &tampon[..n]);
                    }
                    Err(e) => {
                        warn!(erreur = %e, "devialet_air_ecoute_erreur");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }));
        Ok(())
    }

    /// Ferme l'écoute (option désactivée). Les amplis déjà vus expirent.
    pub fn arreter(&self) {
        if let Some(t) = self
            .tache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            t.abort();
            info!("devialet_air_ecoute_fermee");
        }
        self.amplis().clear();
    }
}

impl Drop for Ecoute {
    fn drop(&mut self) {
        if let Ok(mut t) = self.tache.lock()
            && let Some(t) = t.take()
        {
            t.abort();
        }
    }
}

fn ouvrir_socket_d_ecoute(port: u16) -> Result<tokio::net::UdpSocket, String> {
    let s = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .map_err(|e| format!("devialet_air: socket: {e}"))?;
    s.set_reuse_address(true).ok();
    #[cfg(unix)]
    s.set_reuse_port(true).ok();
    s.set_broadcast(true).ok();
    s.bind(&socket2::SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        port,
    )))
    .map_err(|e| format!("devialet_air: bind 0.0.0.0:{port}: {e}"))?;
    s.set_nonblocking(true)
        .map_err(|e| format!("devialet_air: nonblocking: {e}"))?;
    let std_socket: std::net::UdpSocket = s.into();
    tokio::net::UdpSocket::from_std(std_socket).map_err(|e| format!("devialet_air: tokio: {e}"))
}
