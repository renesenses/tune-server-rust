//! RAOP timing requests arrive from the receiver, including during SETUP.
//! The control socket answers retransmission requests (0x55) from the
//! history of sent packets, and carries the sync packets (0x54) that the
//! sender emits (#6751, see `airplay_controle.rs`).
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::controle::{ControleRaop, HistoriqueRtp};

pub(super) struct AuxiliaryUdp {
    control: Arc<tokio::net::UdpSocket>,
    control_port: u16,
    timing_port: u16,
    historique: Arc<HistoriqueRtp>,
    responder: tokio::task::JoinHandle<()>,
}

impl AuxiliaryUdp {
    pub(super) async fn bind(peer: IpAddr) -> Result<Self, String> {
        let address = if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let control = Arc::new(
            tokio::net::UdpSocket::bind(address)
                .await
                .map_err(|e| format!("AirPlay control bind: {e}"))?,
        );
        let control_port = control
            .local_addr()
            .map_err(|e| format!("AirPlay control address: {e}"))?
            .port();
        let timing = tokio::net::UdpSocket::bind(address)
            .await
            .map_err(|e| format!("AirPlay timing bind: {e}"))?;
        let timing_port = timing
            .local_addr()
            .map_err(|e| format!("AirPlay timing address: {e}"))?
            .port();
        // #6751 — les demandes de retransmission du récepteur, servies depuis
        // l'historique des paquets émis, restreintes au pair RTSP comme le
        // timing. UNE seule tâche tient les deux sockets : l'arrêter les
        // libère ensemble (les témoins de #2217 rebindent les deux ports dès
        // que le timing est libre).
        let historique = Arc::new(HistoriqueRtp::default());
        let responder = {
            let control = control.clone();
            let historique = historique.clone();
            tokio::spawn(async move {
                // Accept the largest UDP datagram, then reject a wrong length.
                // A short receive buffer can return WSAEMSGSIZE on Windows.
                let mut packet = vec![0_u8; 65_536];
                let mut demande = vec![0_u8; 65_536];
                loop {
                    tokio::select! {
                        recu = timing.recv_from(&mut packet) => {
                            let (length, source) = match recu {
                                Ok(received) => received,
                                Err(error) => {
                                    tracing::warn!(%error, "airplay_timing_receive_failed");
                                    break;
                                }
                            };
                            let received = ntp_now();
                            // Restrict replies to the RTSP peer and the exact timing format.
                            if source.ip() != peer
                                || length != 32
                                || packet[0] != 0x80
                                || packet[1] != 0xd2
                            {
                                continue;
                            }
                            let mut response = [0_u8; 32];
                            response[..4].copy_from_slice(&packet[..4]);
                            response[1] = 0xd3;
                            response[8..16].copy_from_slice(&packet[24..32]);
                            response[16..24].copy_from_slice(&received);
                            response[24..32].copy_from_slice(&ntp_now());
                            if let Err(error) = timing.send_to(&response, source).await {
                                tracing::warn!(%error, "airplay_timing_reply_failed");
                            }
                        }
                        recu = control.recv_from(&mut demande) => {
                            let (length, source) = match recu {
                                Ok(received) => received,
                                Err(error) => {
                                    tracing::warn!(%error, "airplay_control_receive_failed");
                                    break;
                                }
                            };
                            if source.ip() != peer {
                                continue;
                            }
                            super::controle::servir_la_demande(
                                &control,
                                source,
                                &demande[..length],
                                &historique,
                            )
                            .await;
                        }
                    }
                }
            })
        };
        Ok(Self {
            control,
            control_port,
            timing_port,
            historique,
            responder,
        })
    }

    pub(super) fn ports(&self) -> (u16, u16) {
        (self.control_port, self.timing_port)
    }

    /// #6751 — de quoi émettre les sync vers le port de contrôle que le
    /// récepteur a annoncé dans sa réponse au SETUP, et garder les paquets
    /// pour les retransmissions.
    pub(super) fn controle_vers(&self, cible: SocketAddr) -> ControleRaop {
        ControleRaop {
            socket: self.control.clone(),
            cible,
            historique: self.historique.clone(),
        }
    }

    pub(super) async fn close(self) {
        self.responder.abort();
        // Wait until the task drops the timing socket before Stop returns.
        // Drop also aborts it if this close future itself is cancelled.
        let mut this = self;
        let _ = (&mut this.responder).await;
    }
}

impl Drop for AuxiliaryUdp {
    fn drop(&mut self) {
        self.responder.abort();
    }
}

pub(super) fn ntp_now() -> [u8; 8] {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // NTP seconds wrap at the end of an era; the 32-bit fraction is not nanos.
    let seconds = elapsed.as_secs().wrapping_add(2_208_988_800) as u32;
    let fraction = ((u64::from(elapsed.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut value = [0; 8];
    value[..4].copy_from_slice(&seconds.to_be_bytes());
    value[4..].copy_from_slice(&fraction.to_be_bytes());
    value
}
