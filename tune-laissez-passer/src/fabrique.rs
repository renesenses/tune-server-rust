//! Fabrique de laissez-passer et de jeux de clés signés — TESTS SEULEMENT.
//!
//! Le site signe ; le relais et le serveur vérifient. Cette fabrique joue le
//! rôle du site dans les tests (faux site du relais, du serveur) et dans
//! l'outil qui écrit `vecteurs/`. Elle n'est compilée qu'avec la
//! fonctionnalité `fabrique`, qu'aucun binaire livré n'active.

use blake2::Digest;
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair};

use crate::{
    ALG, AlgCle, Confirmation, DocumentJeuDeCles, EMETTEUR, Publication, Revendications, TYP,
    en_b64, en_b64url, message_defi_session,
};

/// La clé de signature en ligne du site.
pub struct SignataireDuSite {
    pub kid: String,
    paire: Ed25519KeyPair,
}

impl SignataireDuSite {
    pub fn depuis_graine(kid: &str, graine: &[u8; 32]) -> Self {
        Self {
            kid: kid.to_string(),
            paire: Ed25519KeyPair::from_seed_unchecked(graine).expect("graine Ed25519"),
        }
    }

    pub fn cle_publique(&self) -> String {
        en_b64url(self.paire.public_key().as_ref())
    }

    /// Signe des revendications avec l'entête normale.
    pub fn signer(&self, r: &Revendications) -> String {
        let entete = serde_json::json!({"alg": ALG, "typ": TYP, "kid": self.kid}).to_string();
        self.signer_brut(&entete, &serde_json::to_string(r).expect("revendications"))
    }

    /// Signe une entête et une charge données telles quelles : pour fabriquer
    /// les cas refusés (mauvais `alg`, `typ` étranger…).
    pub fn signer_brut(&self, entete: &str, charge: &str) -> String {
        let signe = format!(
            "{}.{}",
            en_b64url(entete.as_bytes()),
            en_b64url(charge.as_bytes())
        );
        let signature = self.paire.sign(signe.as_bytes());
        format!("{signe}.{}", en_b64url(signature.as_ref()))
    }
}

/// La clé racine minisign, hors ligne en production.
pub struct Racine {
    id_de_cle: [u8; 8],
    paire: Ed25519KeyPair,
}

impl Racine {
    pub fn depuis_graine(id_de_cle: [u8; 8], graine: &[u8; 32]) -> Self {
        Self {
            id_de_cle,
            paire: Ed25519KeyPair::from_seed_unchecked(graine).expect("graine Ed25519"),
        }
    }

    /// La ligne base64 du `.pub` minisign : `Ed` || id || clé.
    pub fn cle_publique_minisign(&self) -> String {
        let mut octets = Vec::with_capacity(42);
        octets.extend_from_slice(b"Ed");
        octets.extend_from_slice(&self.id_de_cle);
        octets.extend_from_slice(self.paire.public_key().as_ref());
        en_b64(&octets)
    }

    /// Le contenu d'un `.minisig` prédigéré (BLAKE2b-512), comme `minisign -S`.
    pub fn signer_minisign(&self, donnees: &[u8], commentaire: &str) -> String {
        let empreinte = blake2::Blake2b512::digest(donnees);
        let signature = self.paire.sign(&empreinte);
        let mut bin1 = Vec::with_capacity(74);
        bin1.extend_from_slice(b"ED");
        bin1.extend_from_slice(&self.id_de_cle);
        bin1.extend_from_slice(signature.as_ref());
        let mut globale = signature.as_ref().to_vec();
        globale.extend_from_slice(commentaire.as_bytes());
        let signature_globale = self.paire.sign(&globale);
        format!(
            "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {}\n{}\n",
            en_b64(&bin1),
            commentaire,
            en_b64(signature_globale.as_ref())
        )
    }

    /// Signe un jeu de clés et rend la publication.
    pub fn publier(&self, doc: &DocumentJeuDeCles) -> Publication {
        let jeu = serde_json::to_string(doc).expect("jeu de cles");
        let minisig = self.signer_minisign(jeu.as_bytes(), "tune-remote jeu de cles");
        Publication { jeu, minisig }
    }
}

/// Un appareil de test : Ed25519 ou P-256.
pub enum AppareilDeTest {
    Ed25519(Ed25519KeyPair),
    P256(EcdsaKeyPair),
}

impl AppareilDeTest {
    pub fn ed25519(graine: &[u8; 32]) -> Self {
        Self::Ed25519(Ed25519KeyPair::from_seed_unchecked(graine).expect("graine Ed25519"))
    }

    /// Une paire P-256 lue d'un PKCS#8 (DER).
    pub fn p256(pkcs8: &[u8]) -> Self {
        let rng = ring::rand::SystemRandom::new();
        Self::P256(
            EcdsaKeyPair::from_pkcs8(
                &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                pkcs8,
                &rng,
            )
            .expect("pkcs8 P-256"),
        )
    }

    /// Un PKCS#8 P-256 neuf.
    pub fn nouveau_pkcs8_p256() -> Vec<u8> {
        let rng = ring::rand::SystemRandom::new();
        EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("pkcs8 P-256")
            .as_ref()
            .to_vec()
    }

    pub fn confirmation(&self) -> Confirmation {
        match self {
            Self::Ed25519(p) => Confirmation {
                alg: AlgCle::Ed25519,
                pk: en_b64url(p.public_key().as_ref()),
            },
            Self::P256(p) => Confirmation {
                alg: AlgCle::P256,
                pk: en_b64url(p.public_key().as_ref()),
            },
        }
    }

    pub fn signer(&self, message: &[u8]) -> String {
        match self {
            Self::Ed25519(p) => en_b64url(p.sign(message).as_ref()),
            Self::P256(p) => {
                let rng = ring::rand::SystemRandom::new();
                en_b64url(p.sign(&rng, message).expect("signature P-256").as_ref())
            }
        }
    }

    /// Signe le défi de session.
    pub fn signer_defi(&self, server_id: &str, defi: &str) -> String {
        self.signer(&message_defi_session(server_id, defi))
    }
}

/// Des revendications valables d'`iat` à `iat + 3600`.
pub fn revendications(
    sub: &str,
    sid: &str,
    did: &str,
    cnf: Confirmation,
    iat: i64,
    jti: &str,
) -> Revendications {
    Revendications {
        iss: EMETTEUR.to_string(),
        aud: crate::AUDIENCE.to_string(),
        sub: sub.to_string(),
        sid: sid.to_string(),
        did: did.to_string(),
        cnf,
        p2p: false,
        srv_pk: None,
        iat,
        nbf: iat,
        exp: iat + crate::DUREE_MAX_S,
        jti: jti.to_string(),
        v: crate::VERSION_FORMAT,
    }
}

/// Les clés de test PUBLIÉES dans `vecteurs/cles_de_test.json`. Elles servent
/// aux tests du relais et du serveur : jamais en production.
pub mod cles_de_test {
    use super::*;

    pub const KID: &str = "test-2026-10";
    pub const GRAINE_SITE: [u8; 32] = [0x51; 32];
    pub const GRAINE_RACINE: [u8; 32] = [0x52; 32];
    pub const ID_RACINE: [u8; 8] = *b"tuneTEST";
    pub const GRAINE_AUTRE_RACINE: [u8; 32] = [0x53; 32];
    pub const GRAINE_APPAREIL: [u8; 32] = [0x54; 32];
    pub const GRAINE_AUTRE_APPAREIL: [u8; 32] = [0x55; 32];
    pub const GRAINE_SERVEUR: [u8; 32] = [0x56; 32];
    /// Début de validité de la clé de test : 01/10/2026 00:00 UTC.
    pub const VALABLE_DU: i64 = 1_790_812_800;
    /// Fin de validité de la clé de test : 01/10/2036 00:00 UTC.
    pub const VALABLE_AU: i64 = 2_106_345_600;

    pub fn site() -> SignataireDuSite {
        SignataireDuSite::depuis_graine(KID, &GRAINE_SITE)
    }

    pub fn racine() -> Racine {
        Racine::depuis_graine(ID_RACINE, &GRAINE_RACINE)
    }

    pub fn document(version: u64) -> DocumentJeuDeCles {
        DocumentJeuDeCles {
            v: crate::VERSION_FORMAT,
            version,
            iss: EMETTEUR.to_string(),
            cles: vec![crate::CleDuSite {
                kid: KID.to_string(),
                alg: ALG.to_string(),
                pk: site().cle_publique(),
                valable_du: VALABLE_DU,
                valable_au: VALABLE_AU,
            }],
        }
    }

    /// Le jeu de clés de test, signé par la racine de test.
    pub fn publication() -> Publication {
        racine().publier(&document(1))
    }
}
