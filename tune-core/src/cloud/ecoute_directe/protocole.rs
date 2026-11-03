//! Protocole du flux de contrôle de l'écoute directe (#6785).
//!
//! Sur une connexion QUIC (iroh) d'ALPN [`ALPN`], le PREMIER flux
//! bidirectionnel ouvert par l'app est le flux de contrôle. Trames : longueur
//! sur 4 octets gros-boutiens, puis un objet JSON d'au plus [`TRAME_MAX`]
//! octets, discriminé par `type`.
//!
//! ```text
//! app → serveur   {"type":"bonjour","v":1}
//! serveur → app   {"type":"defi","v":1,"server_id":"…","defi":"…"}
//! app → serveur   {"type":"ouvrir","laissez_passer":"…","signature":"…"}
//! serveur → app   {"type":"ouvert","expire":…,"secours":false,"defi_suivant":"…"}
//!               | {"type":"refus","code":"…","motif":"…"?}   puis fermeture
//! … plus tard …
//! app → serveur   {"type":"renouveler","laissez_passer":"…","signature":"…"}
//! serveur → app   {"type":"renouvele","expire":…,"defi_suivant":"…"}
//!               | {"type":"coupee","motif":"…"}              puis fermeture
//! serveur → app   {"type":"coupee","motif":"…","appareil":"…"?} puis fermeture
//! ```
//!
//! La signature est celle de [`tune_laissez_passer::message_defi_direct`]
//! (`tune-remote/direct/v1\n<server_id>\n<défi>`) par la clé de l'appareil
//! (`cnf`). Chaque défi ne sert qu'une fois : le renouvellement signe le
//! `defi_suivant` de la réponse précédente.
//!
//! Une fois la session ouverte, CHAQUE autre flux bidirectionnel ouvert par
//! l'app porte une connexion HTTP/1.1 (requêtes successives permises), vers
//! les seules routes que le relais sert : `/api/v1/…` et `/stream/…`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ALPN de l'écoute directe. Toute autre ALPN est refusée à la poignée de
/// main TLS (l'endpoint n'en annonce pas d'autre).
pub const ALPN: &[u8] = b"tune-ecoute/1";
/// Version du protocole de contrôle.
pub const VERSION: u32 = 1;
/// Taille maximale d'une trame de contrôle. Un laissez-passer fait 4 Kio au
/// plus ([`tune_laissez_passer::TAILLE_MAX`]).
pub const TRAME_MAX: usize = 16 * 1024;

/// Code de fermeture QUIC : fin normale.
pub const CODE_FIN: u32 = 0;
/// Code de fermeture QUIC : refus à l'ouverture (raison = le code de refus).
pub const CODE_REFUS: u32 = 4003;
/// Code de fermeture QUIC : session coupée (raison = `{"code":"session_coupee",
/// "motif":…}`), le même code que le WebSocket du relais.
pub const CODE_COUPEE: u32 = 4001;

/// Ce que l'app envoie sur le flux de contrôle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageApp {
    Bonjour {
        v: u32,
    },
    Ouvrir {
        laissez_passer: String,
        signature: String,
    },
    Renouveler {
        laissez_passer: String,
        signature: String,
    },
}

/// Ce que le serveur envoie sur le flux de contrôle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageServeur {
    Defi {
        v: u32,
        server_id: String,
        defi: String,
    },
    Ouvert {
        expire: i64,
        secours: bool,
        defi_suivant: String,
    },
    Renouvele {
        expire: i64,
        defi_suivant: String,
    },
    Refus {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        motif: Option<String>,
    },
    Coupee {
        motif: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        appareil: Option<String>,
    },
}

/// Écrit une trame.
pub async fn ecrire_trame<W, T>(w: &mut W, valeur: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let octets = serde_json::to_vec(valeur)?;
    if octets.len() > TRAME_MAX {
        return Err(std::io::Error::other("trame trop longue"));
    }
    w.write_all(&(octets.len() as u32).to_be_bytes()).await?;
    w.write_all(&octets).await?;
    w.flush().await
}

/// Lit une trame. Une trame plus longue que [`TRAME_MAX`] est refusée avant
/// d'être lue.
pub async fn lire_trame<R, T>(r: &mut R) -> Result<T, String>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut longueur = [0u8; 4];
    r.read_exact(&mut longueur)
        .await
        .map_err(|e| format!("lecture : {e}"))?;
    let longueur = u32::from_be_bytes(longueur) as usize;
    if longueur > TRAME_MAX {
        return Err(format!("trame trop longue ({longueur} octets)"));
    }
    let mut octets = vec![0u8; longueur];
    r.read_exact(&mut octets)
        .await
        .map_err(|e| format!("lecture : {e}"))?;
    serde_json::from_slice(&octets).map_err(|_| "trame illisible".to_string())
}
