//! #6751 — le canal de CONTRÔLE RAOP : paquets de synchronisation et
//! réponses aux demandes de retransmission.
//!
//! Calum Loudon (fil 2261, Marantz NR1602 en AirPlay 1) : l'audio part à
//! 125 paquets/s, mais aucun paquet sync n'atteint le port de contrôle du
//! récepteur, et ses demandes de retransmission (0x55, toutes les 100 ms,
//! toujours le même numéro) restent sans réponse. Le récepteur affiche
//! « Now playing 0:00 » et ne joue rien : sans sync, il n'ancre pas le flux
//! dans le temps.
//!
//! Formats (RAOP, tels que les lisent les récepteurs et shairport-sync) :
//!
//! - **sync** (0x54), émetteur → port de contrôle du récepteur, 20 octets :
//!   `0x80` (`0x90` pour le premier, bit d'extension), `0xd4`, `0x0007`,
//!   l'horodatage RTP moins la latence, l'heure NTP, l'horodatage RTP du
//!   paquet qui part. Le récepteur en tire : « l'échantillon `rtp - latence`
//!   joue à cette heure NTP ».
//! - **demande de retransmission** (0x55), récepteur → nous, 8 octets :
//!   `0x80`, `0xd5`, un numéro, le premier numéro de séquence manquant, le
//!   nombre de paquets.
//! - **retransmission** (0x56), nous → récepteur : `0x80`, `0xd6`, le numéro
//!   du paquet, puis le paquet RTP d'origine en entier.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tracing::{debug, info};

use super::{RTP_HEADER_SIZE, SAMPLE_RATE, SortieRtp};

/// La latence annoncée dans chaque sync, en trames : deux secondes, la valeur
/// d'iTunes que les récepteurs AirPlay 1 attendent.
pub(super) const LATENCE_EN_TRAMES: u32 = 2 * SAMPLE_RATE;
/// Un sync par seconde de flux, comme les émetteurs de référence.
const TRAMES_ENTRE_DEUX_SYNC: u32 = SAMPLE_RATE;
/// Paquets gardés pour les retransmissions : ~8 s à 125 paquets/s, plus que
/// la latence annoncée.
const PAQUETS_GARDES: usize = 1_024;
/// Plafond d'une demande : un récepteur qui réclame davantage ne sera de
/// toute façon pas rattrapé à temps.
const RETRANSMISSION_MAX: u16 = 128;

/// Les derniers paquets RTP émis, par numéro de séquence.
#[derive(Default)]
pub(super) struct HistoriqueRtp(Mutex<VecDeque<(u16, Vec<u8>)>>);

impl HistoriqueRtp {
    pub(super) fn garder(&self, sequence: u16, paquet: &[u8]) {
        let mut h = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if h.len() == PAQUETS_GARDES {
            h.pop_front();
        }
        h.push_back((sequence, paquet.to_vec()));
    }

    pub(super) fn retrouver(&self, sequence: u16) -> Option<Vec<u8>> {
        let h = self.0.lock().unwrap_or_else(|e| e.into_inner());
        h.iter()
            .rev()
            .find(|(s, _)| *s == sequence)
            .map(|(_, p)| p.clone())
    }
}

/// Le paquet sync pour un paquet audio d'horodatage `rtp` émis à `ntp`.
pub(super) fn paquet_sync(premier: bool, rtp: u32, ntp: [u8; 8]) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = if premier { 0x90 } else { 0x80 };
    p[1] = 0xd4;
    p[2..4].copy_from_slice(&7u16.to_be_bytes());
    p[4..8].copy_from_slice(&rtp.wrapping_sub(LATENCE_EN_TRAMES).to_be_bytes());
    p[8..16].copy_from_slice(&ntp);
    p[16..20].copy_from_slice(&rtp.to_be_bytes());
    p
}

/// `(premier numéro, nombre)` d'une demande de retransmission 0x55.
pub(super) fn demande_de_retransmission(paquet: &[u8]) -> Option<(u16, u16)> {
    if paquet.len() < 8 || paquet[1] & 0x7f != 0x55 {
        return None;
    }
    let premier = u16::from_be_bytes([paquet[4], paquet[5]]);
    let nombre = u16::from_be_bytes([paquet[6], paquet[7]]);
    Some((premier, nombre.min(RETRANSMISSION_MAX)))
}

/// La réponse 0x56 qui porte le paquet RTP d'origine.
pub(super) fn reponse_de_retransmission(sequence: u16, original: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(4 + original.len());
    r.extend_from_slice(&[0x80, 0xd6]);
    r.extend_from_slice(&sequence.to_be_bytes());
    r.extend_from_slice(original);
    r
}

/// Sert une demande reçue sur le socket de contrôle. Rend `(servis, absents)`.
pub(super) async fn servir_la_demande(
    socket: &tokio::net::UdpSocket,
    source: SocketAddr,
    demande: &[u8],
    historique: &HistoriqueRtp,
) -> Option<(u16, u16)> {
    let (premier, nombre) = demande_de_retransmission(demande)?;
    let (mut servis, mut absents) = (0u16, 0u16);
    for i in 0..nombre {
        let sequence = premier.wrapping_add(i);
        match historique.retrouver(sequence) {
            Some(original) => {
                let reponse = reponse_de_retransmission(sequence, &original);
                if let Err(e) = socket.send_to(&reponse, source).await {
                    debug!(error = %e, "airplay_retransmission_envoi_echoue");
                }
                servis += 1;
            }
            None => absents += 1,
        }
    }
    debug!(
        premier,
        nombre, servis, absents, "airplay_retransmission_servie"
    );
    Some((servis, absents))
}

/// Ce dont l'émetteur a besoin pour parler au port de contrôle du récepteur.
pub(super) struct ControleRaop {
    pub(super) socket: Arc<tokio::net::UdpSocket>,
    pub(super) cible: SocketAddr,
    pub(super) historique: Arc<HistoriqueRtp>,
}

/// La sortie RTP de production : l'audio sur son socket, et, quand le
/// récepteur a annoncé son port de contrôle, les sync avant le premier paquet
/// puis une fois par seconde, et l'historique pour les retransmissions.
pub(super) struct EmetteurRaop {
    audio: tokio::net::UdpSocket,
    controle: Option<ControleRaop>,
    dernier_sync: Mutex<Option<u32>>,
}

impl EmetteurRaop {
    pub(super) fn new(audio: tokio::net::UdpSocket, controle: Option<ControleRaop>) -> Self {
        if controle.is_none() {
            info!("airplay_sans_port_de_controle_aucun_sync");
        }
        Self {
            audio,
            controle,
            dernier_sync: Mutex::new(None),
        }
    }

    /// Faut-il un sync avant ce paquet ? Toujours sur un marqueur (premier
    /// paquet après RECORD ou FLUSH), puis toutes les secondes de flux.
    fn sync_du(&self, marqueur: bool, rtp: u32) -> Option<bool> {
        let mut dernier = self.dernier_sync.lock().unwrap_or_else(|e| e.into_inner());
        let du = match *dernier {
            _ if marqueur => true,
            None => true,
            Some(d) => rtp.wrapping_sub(d) >= TRAMES_ENTRE_DEUX_SYNC,
        };
        if !du {
            return None;
        }
        *dernier = Some(rtp);
        Some(marqueur)
    }
}

impl SortieRtp for EmetteurRaop {
    async fn envoyer(&self, pkt: &[u8]) {
        if pkt.len() >= RTP_HEADER_SIZE
            && let Some(controle) = &self.controle
        {
            let marqueur = pkt[1] & 0x80 != 0;
            let sequence = u16::from_be_bytes([pkt[2], pkt[3]]);
            let rtp = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
            if let Some(premier) = self.sync_du(marqueur, rtp) {
                let sync = paquet_sync(premier, rtp, super::timing::ntp_now());
                if let Err(e) = controle.socket.send_to(&sync, controle.cible).await {
                    debug!(error = %e, "airplay_sync_envoi_echoue");
                } else if premier {
                    info!(rtp, cible = %controle.cible, "airplay_premier_sync_envoye");
                }
            }
            controle.historique.garder(sequence, pkt);
        }
        if let Err(e) = self.audio.send(pkt).await {
            debug!(error = %e, "airplay_rtp_send_error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_paquet_sync_porte_la_latence_et_les_deux_horodatages() {
        let ntp = [1, 2, 3, 4, 5, 6, 7, 8];
        let p = paquet_sync(true, 100_000, ntp);
        assert_eq!(&p[..4], &[0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(
            u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
            100_000 - LATENCE_EN_TRAMES
        );
        assert_eq!(&p[8..16], &ntp);
        assert_eq!(u32::from_be_bytes([p[16], p[17], p[18], p[19]]), 100_000);
        assert_eq!(paquet_sync(false, 5, ntp)[0], 0x80);
        // Début de flux : l'horodatage moins la latence reboucle, sans panique.
        let debut = paquet_sync(true, 0, ntp);
        assert_eq!(
            u32::from_be_bytes([debut[4], debut[5], debut[6], debut[7]]),
            0u32.wrapping_sub(LATENCE_EN_TRAMES)
        );
    }

    #[test]
    fn une_demande_0x55_se_lit_et_se_borne() {
        let demande = [0x80, 0xd5, 0x00, 0x01, 0x22, 0x4f, 0x00, 0x01];
        assert_eq!(demande_de_retransmission(&demande), Some((8783, 1)));
        let enorme = [0x80, 0xd5, 0, 1, 0, 0, 0xff, 0xff];
        assert_eq!(
            demande_de_retransmission(&enorme),
            Some((0, RETRANSMISSION_MAX))
        );
        assert_eq!(
            demande_de_retransmission(&[0x80, 0xd4, 0, 7, 0, 0, 0, 1]),
            None
        );
        assert_eq!(demande_de_retransmission(&[0x80, 0xd5]), None);
    }

    #[test]
    fn l_historique_garde_les_derniers_paquets() {
        let h = HistoriqueRtp::default();
        for s in 0..(PAQUETS_GARDES as u16 + 10) {
            h.garder(s, &[s as u8]);
        }
        assert_eq!(h.retrouver(5), None, "les plus anciens sont sortis");
        assert_eq!(
            h.retrouver(PAQUETS_GARDES as u16 + 9),
            Some(vec![(PAQUETS_GARDES as u16 + 9) as u8])
        );
    }
}
