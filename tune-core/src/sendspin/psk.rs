//! PSK Sendspin : categorie de confiance et liaison a une identite de pair.
//!
//! La sentinelle est publique. Une PSK d'appairage ou longue duree ne doit
//! jamais etre choisie pour une autre cle publique, meme si son secret coincide.

use sha2::{Digest, Sha256};

use super::identite::b64url;

/// Taille d'une PSK Noise. Fixée par le cadre, pas par nous.
pub const TAILLE_PSK: usize = 32;

/// L'étiquette de domaine qui préfixe le condensé d'une PSK pour en faire un
/// `psk_id`. Séparer les domaines évite qu'un condensé calculé ailleurs dans le
/// protocole puisse être présenté comme un `psk_id`.
pub const ETIQUETTE_PSK_ID: &[u8] = b"sendspin-psk-id-v1";

/// La graine dont la Sentinelle est le condensé SHA-256.
pub const GRAINE_SENTINELLE: &[u8] = b"sendspin-sentinel-psk-v1";

/// La PSK Sentinelle : `SHA-256(GRAINE_SENTINELLE)`.
///
/// Publique par construction — la recopier dans un journal n'est pas une fuite.
pub fn sentinelle() -> [u8; TAILLE_PSK] {
    Sha256::digest(GRAINE_SENTINELLE).into()
}

/// Le `psk_id` d'une PSK : `base64url(SHA-256(ETIQUETTE_PSK_ID || psk))`.
///
/// C'est ce que le serveur glisse en charge utile du PREMIER message Noise,
/// pour que l'enceinte sache laquelle de ses PSK mêler au second — le motif
/// `KKpsk2` mêle la PSK au message 2, donc le répondeur a le droit de ne pas la
/// connaître en lisant le message 1.
pub fn identifiant(psk: &[u8; TAILLE_PSK]) -> String {
    let mut hacheur = Sha256::new();
    hacheur.update(ETIQUETTE_PSK_ID);
    hacheur.update(psk);
    b64url(&hacheur.finalize())
}

/// La categorie fait partie de la charge authentifiee du message Noise 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CategoriePsk {
    #[serde(rename = "sn")]
    Sentinelle,
    #[serde(rename = "pr")]
    Appairage,
    #[serde(rename = "lt")]
    LongueDuree,
}

/// Un secret et sa destination. Debug ne montre jamais les octets de la PSK.
#[derive(Clone)]
pub struct PskPair {
    secret: [u8; TAILLE_PSK],
    categorie: CategoriePsk,
    client_id: Option<String>,
}

impl std::fmt::Debug for PskPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PskPair")
            .field("categorie", &self.categorie)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl PskPair {
    pub fn sentinelle() -> Self {
        Self {
            secret: sentinelle(),
            categorie: CategoriePsk::Sentinelle,
            client_id: None,
        }
    }

    pub fn pour_pair(
        client_id: &str,
        secret: [u8; TAILLE_PSK],
        categorie: CategoriePsk,
    ) -> Result<Self, super::ErreurSendspin> {
        super::identite::cle_publique_du_pair(client_id)?;
        if categorie == CategoriePsk::Sentinelle || secret == sentinelle() {
            return Err(super::ErreurSendspin::EtatInattendu(
                "une PSK privee ne peut pas etre la sentinelle publique",
            ));
        }
        Ok(Self {
            secret,
            categorie,
            client_id: Some(client_id.to_owned()),
        })
    }

    pub fn categorie(&self) -> CategoriePsk {
        self.categorie
    }
    pub fn identifiant(&self) -> String {
        identifiant(&self.secret)
    }
    pub fn secret(&self) -> &[u8; TAILLE_PSK] {
        &self.secret
    }

    pub(super) fn verifier_pair(&self, client_id: &str) -> Result<(), super::ErreurSendspin> {
        if self
            .client_id
            .as_deref()
            .is_some_and(|attendu| attendu != client_id)
        {
            return Err(super::ErreurSendspin::EtatInattendu(
                "PSK liee a un autre client",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_sentinelle_est_le_condense_de_sa_graine_et_ne_bouge_pas() {
        let attendu: [u8; TAILLE_PSK] = Sha256::digest(b"sendspin-sentinel-psk-v1").into();
        assert_eq!(
            sentinelle(),
            attendu,
            "la Sentinelle est une constante PUBLIEE : si cette valeur bouge, \
             plus aucune enceinte ne se connecte"
        );
    }

    #[test]
    fn un_psk_id_est_domaine_separe_du_condense_nu() {
        let psk = sentinelle();
        let nu = b64url(&Sha256::digest(psk));
        assert_ne!(
            identifiant(&psk),
            nu,
            "sans l'etiquette de domaine, un condense calcule ailleurs passerait \
             pour un psk_id"
        );
        assert_eq!(
            identifiant(&psk).len(),
            43,
            "un psk_id est un SHA-256 en base64url sans remplissage"
        );
    }

    #[test]
    fn deux_psk_differentes_ont_deux_identifiants_differents() {
        let mut autre = sentinelle();
        autre[0] ^= 0xff;
        assert_ne!(identifiant(&sentinelle()), identifiant(&autre));
    }
}
