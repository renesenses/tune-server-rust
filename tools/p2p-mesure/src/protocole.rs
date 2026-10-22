//! Protocole du banc sur la connexion QUIC (cahier § 2.2).
//!
//! - flux de contrôle bidirectionnel, ouvert par le client : `Bonjour` →
//!   `Reponse`, puis une `Commande` par phase ;
//! - pings horodatés de 16 octets (`seq`, `t_us`) en datagrammes QUIC, ou sur un
//!   second flux bidirectionnel si les datagrammes sont indisponibles ;
//! - un flux unidirectionnel de données par phase (vrac ou cadencé), ouvert par
//!   le serveur. Octets synthétiques seulement : jamais un fichier.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use data_encoding::BASE32_NOPAD;
use iroh::endpoint::{RecvStream, SendStream, VarInt};
use iroh::EndpointAddr;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const ALPN: &[u8] = b"tune-mesure/1";
pub const VERSION: u32 = 1;

/// Taille des blocs de données (cahier § 3.1 : blocs de 64 Kio).
pub const BLOC: usize = 64 * 1024;
/// Tampon de lecture simulé, et rafale initiale de remplissage.
pub const TAMPON_S: f64 = 2.0;
pub const PERIODE_PING: Duration = Duration::from_millis(200);
pub const TAILLE_PING: usize = 16;

/// Bornes côté serveur : un client ne peut pas lui faire servir plus.
pub const VRAC_MAX: Duration = Duration::from_secs(60);
pub const FLUX_MAX: Duration = Duration::from_secs(600);

/// Codes de fermeture applicatifs.
pub const CODE_FIN: u32 = 0;
pub const CODE_IDENTIFIANT_INCONNU: u32 = 1;
pub const CODE_REFUS: u32 = 2;
pub const CODE_ALPN: u32 = 3;
pub const CODE_ARRET: u32 = 4;

pub fn code(c: u32) -> VarInt {
    VarInt::from_u32(c)
}

/// Formats et débits cibles cadencés (cahier § 3.1 : FLAC moyen, majoré).
pub const FORMATS: [(&str, u64); 3] = [
    ("16/44", 1_000_000),
    ("24/96", 3_500_000),
    ("24/192", 7_000_000),
];

pub fn debit_cible(format: &str) -> Option<u64> {
    FORMATS.iter().find(|(f, _)| *f == format).map(|(_, d)| *d)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bonjour {
    pub v: u32,
    pub campagne: String,
    pub testeur: String,
    pub jeton: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InfoServeur {
    pub site: String,
    #[serde(rename = "box")]
    pub box_: String,
    pub famille: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reponse {
    pub ok: bool,
    pub raison: Option<String>,
    pub serveur: Option<InfoServeur>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Commande {
    Vrac { duree_ms: u64 },
    Flux { format: String, duree_ms: u64 },
    Fin,
}

const TRAME_MAX: usize = 16 * 1024;

pub async fn ecrire_trame<T: Serialize>(send: &mut SendStream, valeur: &T) -> Result<()> {
    let octets = serde_json::to_vec(valeur)?;
    send.write_all(&(octets.len() as u32).to_be_bytes()).await?;
    send.write_all(&octets).await?;
    Ok(())
}

pub async fn lire_trame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut longueur = [0u8; 4];
    recv.read_exact(&mut longueur).await?;
    let longueur = u32::from_be_bytes(longueur) as usize;
    if longueur > TRAME_MAX {
        bail!("trame de contrôle trop longue ({longueur} octets)");
    }
    let mut octets = vec![0u8; longueur];
    recv.read_exact(&mut octets).await?;
    Ok(serde_json::from_slice(&octets)?)
}

pub fn ping(seq: u64, t_us: u64) -> [u8; TAILLE_PING] {
    let mut p = [0u8; TAILLE_PING];
    p[..8].copy_from_slice(&seq.to_be_bytes());
    p[8..].copy_from_slice(&t_us.to_be_bytes());
    p
}

pub fn lire_ping(octets: &[u8]) -> Option<(u64, u64)> {
    if octets.len() != TAILLE_PING {
        return None;
    }
    let seq = u64::from_be_bytes(octets[..8].try_into().ok()?);
    let t = u64::from_be_bytes(octets[8..].try_into().ok()?);
    Some((seq, t))
}

/// Ticket : `TM1` + base32 du JSON de l'`EndpointAddr` (identifiant, adresses,
/// URL du relais). Base32 en capitales : mode alphanumérique du QR code.
///
/// Le ticket contient des adresses IP : il passe à la main, n'est publié nulle
/// part (pas de pkarr, pas de DNS d'n0) et n'est jamais écrit dans un journal.
pub fn encoder_ticket(addr: &EndpointAddr) -> Result<String> {
    Ok(format!(
        "TM1{}",
        BASE32_NOPAD.encode(&serde_json::to_vec(addr)?)
    ))
}

pub fn decoder_ticket(ticket: &str) -> Result<EndpointAddr> {
    let corps = ticket
        .trim()
        .strip_prefix("TM1")
        .context("ticket inconnu : il doit commencer par TM1")?;
    let json = BASE32_NOPAD
        .decode(corps.to_ascii_uppercase().as_bytes())
        .context("ticket illisible (base32)")?;
    serde_json::from_slice(&json).context("ticket illisible (JSON)")
}

/// Famille d'une adresse, sans l'adresse. Une IPv4 dans l'IPv6 compte IPv4.
pub fn famille(ip: std::net::IpAddr) -> &'static str {
    match ip {
        std::net::IpAddr::V4(_) => "ipv4",
        std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => "ipv4",
        std::net::IpAddr::V6(_) => "ipv6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{SecretKey, TransportAddr};

    #[test]
    fn ticket_aller_retour() {
        let id = SecretKey::generate().public();
        let addr = EndpointAddr::from_parts(
            id,
            [
                TransportAddr::Ip("127.0.0.1:4433".parse().unwrap()),
                TransportAddr::Relay("https://relay.example.org./".parse().unwrap()),
            ],
        );
        let t = encoder_ticket(&addr).unwrap();
        assert!(t.starts_with("TM1"));
        assert_eq!(decoder_ticket(&t).unwrap(), addr);
        assert!(decoder_ticket("XX1AAAA").is_err());
    }

    #[test]
    fn ping_aller_retour() {
        assert_eq!(lire_ping(&ping(7, 123_456)), Some((7, 123_456)));
        assert_eq!(lire_ping(&[0u8; 3]), None);
    }

    #[test]
    fn familles() {
        assert_eq!(famille("10.0.0.1".parse().unwrap()), "ipv4");
        assert_eq!(famille("::ffff:10.0.0.1".parse().unwrap()), "ipv4");
        assert_eq!(famille("2a01:cb00::1".parse().unwrap()), "ipv6");
    }
}
