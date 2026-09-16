//! Entrees operateur des trois methodes d'appairage Sendspin.
//!
//! Contrat : Sendspin/spec@8a8b1cbd, pairing.md, « Pairing Token ».
//! Les jetons restent secrets. La PSK v0 est inutilisable sans verifier
//! l'identite de la connexion ; le QR v1 fournit 24 octets bruts a CPace.
use zeroize::Zeroizing;

use super::identite::{b64url, cle_publique_du_pair};
use super::pake::{CodeAppairage, ErreurAppairage, FormatCode};
use super::psk::{CategoriePsk, PskPair};

/// PSK provisoire et identite saisies ensemble, sans exposition du secret.
pub struct JetonPsk {
    client_id: String,
    secret: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for JetonPsk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JetonPsk")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl JetonPsk {
    pub fn lire(saisie: &str) -> Result<Self, ErreurAppairage> {
        let charge = decoder(saisie, b'0', 64)?;
        let client_id = b64url(&charge[..32]);
        cle_publique_du_pair(&client_id)
            .map_err(|_| ErreurAppairage::Protocole("identite du jeton"))?;
        Ok(Self {
            client_id,
            secret: Zeroizing::new(charge[32..64].try_into().expect("taille verifiee")),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Le destinataire est celui du transport Noise, jamais un nom mDNS.
    pub fn pour_pair(self, client_id: &str) -> Result<PskPair, ErreurAppairage> {
        if self.client_id != client_id {
            return Err(ErreurAppairage::Protocole("jeton d'un autre client"));
        }
        PskPair::pour_pair(client_id, *self.secret, CategoriePsk::Appairage)
            .map_err(|_| ErreurAppairage::Protocole("PSK du jeton"))
    }
}

/// La presentation groupee des chiffres ne change pas les octets PRS.
/// Un code QR ne passe jamais par la normalisation des chiffres.
pub fn lire_code(saisie: &str, format: FormatCode) -> Result<CodeAppairage, ErreurAppairage> {
    let charge = if format == FormatCode::Qr {
        decoder(saisie, b'1', 24)?
    } else {
        Zeroizing::new(
            saisie
                .trim()
                .bytes()
                .filter(|b| !matches!(b, b' ' | b'-'))
                .collect::<Vec<_>>(),
        )
    };
    CodeAppairage::nouveau(format, &charge)
}

/// Les octets d'extension sont ignores seulement APRES validation de tout
/// l'encodage. L'appelant HTTP bornera la taille de la requete operateur.
fn decoder(
    saisie: &str,
    version: u8,
    taille: usize,
) -> Result<Zeroizing<Vec<u8>>, ErreurAppairage> {
    let saisie = saisie.trim();
    if !saisie.is_ascii() {
        return Err(ErreurAppairage::Protocole("jeton non ASCII"));
    }
    let texte = Zeroizing::new(saisie.to_ascii_uppercase());
    let texte = texte.strip_prefix("SP:").unwrap_or(&texte);
    if texte.as_bytes().first() != Some(&version) {
        return Err(ErreurAppairage::Protocole("version du jeton"));
    }
    let mut corps = Zeroizing::new(texte[1..].replace('9', "2"));
    while corps.len() % 8 != 0 {
        corps.push('=');
    }
    let mut charge = Zeroizing::new(
        data_encoding::BASE32
            .decode(corps.as_bytes())
            .map_err(|_| ErreurAppairage::Protocole("encodage du jeton"))?,
    );
    if charge.len() < taille {
        return Err(ErreurAppairage::Protocole("jeton tronque"));
    }
    // Ne conserver aucune extension secrete dans le resultat.
    use zeroize::Zeroize as _;
    charge[taille..].zeroize();
    charge.truncate(taille);
    Ok(charge)
}

#[cfg(test)]
#[path = "jeton/tests.rs"]
mod tests;
