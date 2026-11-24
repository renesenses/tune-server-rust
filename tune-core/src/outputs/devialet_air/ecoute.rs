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
use tracing::{debug, error, info, warn};

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

/// #9380 — combien de formes inconnues sont journalisées, au plus.
pub const FORMES_JOURNALISEES_AU_PLUS: usize = 8;

/// #9380 — un datagramme reçu sur le port des annonces mais écarté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatagrammeEcarte {
    pub ip: IpAddr,
    pub taille: usize,
    /// Les deux premiers octets, en texte quand ils sont imprimables
    /// (`Da`), en hexadécimal sinon.
    pub entete: String,
    pub raison: String,
}

/// #9380 — ce que l'écoute a reçu depuis son ouverture.
///
/// Sans ce bilan, « aucun ampli » ne disait rien : rien reçu du tout (le
/// réseau ou le système ne laisse rien passer) et des annonces reçues mais
/// d'une forme que Tune ne connaît pas (un autre modèle, un autre
/// micrologiciel) donnaient le même `0 device(s)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BilanEcoute {
    pub datagrammes: u64,
    pub annonces: u64,
    pub ecartes: u64,
    pub dernier_ecarte: Option<DatagrammeEcarte>,
    /// L'erreur de la dernière ouverture refusée, effacée par une ouverture
    /// réussie.
    pub ouverture_refusee: Option<String>,
}

/// L'état des amplis entendus, par MAC.
#[derive(Default)]
pub struct Ecoute {
    amplis: Mutex<HashMap<String, AmpliVu>>,
    tache: Mutex<Option<JoinHandle<()>>>,
    bilan: Mutex<BilanEcoute>,
    /// (ip, taille, en-tête) déjà journalisés, pour ne le dire qu'une fois.
    formes_dites: Mutex<Vec<(IpAddr, usize, String)>>,
}

/// Les deux premiers octets d'un datagramme, lisibles dans un journal.
fn entete_lisible(octets: &[u8]) -> String {
    let tete = &octets[..octets.len().min(2)];
    if !tete.is_empty() && tete.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(tete).into_owned()
    } else {
        tete.iter().map(|b| format!("{b:02x}")).collect()
    }
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
        self.bilan_mut().datagrammes += 1;
        let annonce = match analyser_annonce(octets) {
            Ok(a) => a,
            Err(e) => {
                self.ecarter(ip, octets, &e.to_string());
                return None;
            }
        };
        self.bilan_mut().annonces += 1;
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

    fn bilan_mut(&self) -> std::sync::MutexGuard<'_, BilanEcoute> {
        self.bilan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// #9380 — un datagramme écarté est compté, et chaque forme nouvelle
    /// (source, taille, en-tête) est dite UNE fois au-dessus de `debug` : une
    /// annonce de 345 octets au CRC faux ne laissait qu'un `debug`, et toute
    /// autre taille rien du tout. Un ampli qui s'annonce dans une forme
    /// inconnue de Tune restait donc invisible, sans une trace.
    fn ecarter(&self, ip: IpAddr, octets: &[u8], raison: &str) {
        let ecarte = DatagrammeEcarte {
            ip,
            taille: octets.len(),
            entete: entete_lisible(octets),
            raison: raison.to_string(),
        };
        {
            let mut bilan = self.bilan_mut();
            bilan.ecartes += 1;
            bilan.dernier_ecarte = Some(ecarte.clone());
        }
        let forme = (ip, ecarte.taille, ecarte.entete.clone());
        let nouvelle = {
            let mut dites = self
                .formes_dites
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if dites.contains(&forme) || dites.len() >= FORMES_JOURNALISEES_AU_PLUS {
                false
            } else {
                dites.push(forme);
                true
            }
        };
        if nouvelle {
            info!(
                %ip,
                taille = ecarte.taille,
                attendue = TAILLE_ANNONCE,
                entete = %ecarte.entete,
                raison = %ecarte.raison,
                "devialet_air_datagramme_ecarte"
            );
        } else {
            debug!(%ip, taille = ecarte.taille, raison = %ecarte.raison, "devialet_air_datagramme_ecarte");
        }
    }

    /// #9380 — ce que l'écoute a reçu depuis son ouverture.
    #[must_use]
    pub fn bilan(&self) -> BilanEcoute {
        self.bilan_mut().clone()
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
        // #8536 — un échec d'ouverture ne reste jamais muet : la zone
        // n'apparaîtrait pas, sans rien dans les journaux pour dire pourquoi.
        let socket = ouvrir_socket_d_ecoute(port).inspect_err(|e| {
            error!(port, erreur = %e, "devialet_air_ecoute_ouverture_refusee");
            self.bilan_mut().ouverture_refusee = Some(e.clone());
        })?;
        info!(port, "devialet_air_ecoute_ouverte");
        self.bilan_mut().ouverture_refusee = None;
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
        // #9380 — le bilan vaut pour UNE ouverture : celui d'une écoute
        // fermée ne décrirait plus rien.
        *self.bilan_mut() = BilanEcoute::default();
        self.formes_dites
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
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
    // #8536 — RoonServer garde 45454 ouvert en permanence. SO_REUSEADDR (et,
    // sous Unix, SO_REUSEPORT : macOS l'exige pour partager un port de
    // diffusion, Linux l'accepte) permet de l'écouter à côté de lui. Sous
    // Windows, SO_REUSEADDR seul suffit. Un refus d'option se journalise :
    // c'est lui qui ferait échouer l'ouverture qui suit.
    if let Err(e) = s.set_reuse_address(true) {
        warn!(port, erreur = %e, "devialet_air_ecoute_so_reuseaddr_refuse");
    }
    #[cfg(unix)]
    if let Err(e) = s.set_reuse_port(true) {
        warn!(port, erreur = %e, "devialet_air_ecoute_so_reuseport_refuse");
    }
    if let Err(e) = s.set_broadcast(true) {
        warn!(port, erreur = %e, "devialet_air_ecoute_so_broadcast_refuse");
    }
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
