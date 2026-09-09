//! Les clés pré-partagées, et **seulement** celle que S2-a a le droit
//! d'employer.
//!
//! Le protocole connaît trois catégories de PSK : `lt` (longue durée, née d'un
//! appairage), `pr` (appairage en cours) et `sn` (**Sentinelle**). Les deux
//! premières sont le sujet de S2-b et n'ont aucune place ici.
//!
//! La Sentinelle est une **constante publiée** : elle est identique chez tous
//! les pairs, donc elle n'authentifie personne. Elle sert à ce que la couche
//! Noise ait une PSK à mêler quand aucune autre ne s'applique — c'est-à-dire
//! exactement la situation de S2-a, qui monte le tuyau chiffré avant que
//! l'appairage n'existe. Le chiffrement et l'intégrité sont réels ; **la preuve
//! d'identité, elle, ne l'est pas encore**, et c'est ce que S2-b apportera.

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
