//! Laissez-passer d'écoute à distance lié au compte mozaiklabs (#6603).
//!
//! L'écoute à distance passe par le compte du propriétaire du serveur. Le site
//! délivre un laissez-passer d'une heure, lié au compte (`sub`), au serveur
//! (`sid`), à l'appareil (`did`) et à la clé publique de cet appareil (`cnf`).
//! Le relais, et demain le serveur en direct, le vérifient HORS LIGNE avec la
//! clé publique du site, puis demandent à l'appareil de signer un défi : le
//! laissez-passer seul ne suffit pas, il faut aussi la clé privée.
//!
//! Cette caisse est la seule règle de vérification, partagée par `tune-bridge`
//! et `tune-core`. Le site PHP rejoue les mêmes fichiers de `vecteurs/`.
//!
//! # Format
//!
//! Un JWS compact (`entête.charge.signature`, base64url sans remplissage) :
//!
//! - entête : exactement `{"alg":"EdDSA","typ":"tune-pass+jwt","kid":"…"}`.
//!   Tout autre `alg` (`none`, `HS256`, `ES256`…) est refusé AVANT de regarder
//!   la signature, et tout champ inconnu de l'entête (`jwk`, `jku`, `crit`…)
//!   aussi : la clé de vérification vient du jeu de clés, jamais du jeton ;
//! - charge : voir [`Revendications`] ;
//! - signature : Ed25519 par la clé de signature du site désignée par `kid`.
//!
//! # Jeu de clés
//!
//! Les clés de signature du site sont publiées dans un jeu de clés
//! ([`Publication`]) signé par une clé RACINE minisign, gardée hors ligne. Le
//! relais et le serveur n'acceptent un jeu que s'il porte la signature de la
//! racine. Sans jeu de clés, rien ne passe : il n'y a pas de repli permissif.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

#[cfg(feature = "fabrique")]
pub mod fabrique;

/// `typ` de l'entête. Distinct de tout autre jeton signé par le site.
pub const TYP: &str = "tune-pass+jwt";
/// Seul algorithme de signature accepté pour un laissez-passer.
pub const ALG: &str = "EdDSA";
/// Émetteur attendu (`iss`).
pub const EMETTEUR: &str = "https://mozaiklabs.fr";
/// Audience attendue (`aud`) : empêche de réutiliser un autre jeton du site.
pub const AUDIENCE: &str = "tune-remote";
/// Version du format (`v`).
pub const VERSION_FORMAT: u32 = 1;
/// Durée de vie maximale d'un laissez-passer : `exp - iat` au plus.
pub const DUREE_MAX_S: i64 = 3600;
/// Tolérance d'horloge sur `iat`, `nbf` et `exp`.
pub const TOLERANCE_HORLOGE_S: i64 = 120;
/// Taille maximale d'un laissez-passer, en octets. Un jeton plus long est
/// refusé avant tout décodage.
pub const TAILLE_MAX: usize = 4096;

/// Clé publique RACINE minisign (la ligne base64 du fichier `.pub`).
///
/// Générée par Bertrand le 22/10/2026 ; la clé secrète reste hors ligne, sur
/// son poste. Elle signe le jeu de clés publié par le site
/// (`GET /api/v1/remote/keys`). Le relais peut la remplacer par la variable
/// `TUNE_REMOTE_RACINE` (rotation de la racine). Copie publique et jeu signé
/// du jour : `vecteurs/jeu-publie-20261022/`.
pub const RACINE_EMBARQUEE: &str = "RWTS2M4Wa1nnskW3GczP9flXOlSejFnyH3g6bqtb1R1ubq9veWi2sGTp";

/// Préfixe du message que l'appareil signe pour ouvrir une session.
pub const DOMAINE_DEFI_SESSION: &str = "tune-remote/session/v1";
/// Préfixe du message que le serveur signe pour s'enregistrer sur le relais.
pub const DOMAINE_DEFI_SERVEUR: &str = "tune-remote/serveur/v1";
/// Préfixe du message que le serveur signe pour inscrire sa clé sur le compte.
pub const DOMAINE_INSCRIPTION_SERVEUR: &str = "tune-remote/inscription/v1";
/// Préfixe du message que l'appareil signe pour ouvrir (ou renouveler) une
/// session DIRECTE, pair-à-pair, sur le serveur lui-même (#6785). Distinct de
/// [`DOMAINE_DEFI_SESSION`] : une signature donnée au relais ne vaut pas en
/// direct, ni l'inverse.
pub const DOMAINE_DEFI_DIRECT: &str = "tune-remote/direct/v1";
/// Préfixe du message que le serveur signe pour chaque requête qu'il adresse
/// au site au titre du chemin direct (ouverture, battement) (#6785).
pub const DOMAINE_REQUETE_SERVEUR: &str = "tune-remote/requete-serveur/v1";
/// Écart d'horloge toléré par le site sur l'horodatage d'une requête signée
/// du serveur.
pub const TOLERANCE_REQUETE_SERVEUR_S: i64 = 120;

/// Le jeu de clés publié le 22/10/2026, signé par la racine de production :
/// la copie embarquée à la compilation, pour qu'un serveur qui démarre sans
/// réseau sache vérifier tout de suite (cahier, § 2.4). Le jeu relu au site
/// le remplace dès qu'il est plus récent.
pub fn publication_embarquee() -> Publication {
    Publication {
        jeu: include_str!("../vecteurs/jeu-publie-20261022/jeu.json").to_string(),
        minisig: include_str!("../vecteurs/jeu-publie-20261022/jeu.json.minisig").to_string(),
    }
}

/// Pourquoi un laissez-passer, un défi ou un jeu de clés est refusé.
///
/// [`Refus::code`] est le contrat partagé avec le site : les vecteurs de test
/// portent ces codes, et le site doit refuser avec les mêmes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refus {
    /// Jeton mal formé : nombre de segments, base64url, JSON, taille.
    Format,
    /// `alg` de l'entête autre que `EdDSA`.
    Alg,
    /// `typ` de l'entête autre que `tune-pass+jwt`.
    Typ,
    /// `kid` absent du jeu de clés.
    KidInconnu,
    /// La clé désignée par `kid` n'est pas valable à cette date.
    CleHorsValidite,
    /// Signature du site fausse.
    Signature,
    /// `v` autre que [`VERSION_FORMAT`].
    Version,
    /// `iss` autre que [`EMETTEUR`].
    Emetteur,
    /// `aud` autre que [`AUDIENCE`].
    Audience,
    /// `sid` autre que le serveur visé.
    Serveur,
    /// `nbf` (ou `iat`) dans le futur, au-delà de la tolérance.
    PasEncoreValide,
    /// `exp` dépassé, au-delà de la tolérance.
    Expire,
    /// `exp - iat` plus long que [`DUREE_MAX_S`], ou négatif.
    Duree,
    /// `cnf` illisible : algorithme inconnu ou clé de mauvaise taille.
    CleAppareil,
    /// Signature du défi absente, illisible ou faite par une autre clé.
    Defi,
    /// Aucune racine configurée : aucun jeu de clés ne peut être accepté.
    RacineAbsente,
    /// Jeu de clés non signé par la racine.
    JeuDeClesSignature,
    /// Jeu de clés illisible.
    JeuDeClesFormat,
}

impl Refus {
    /// Code stable, partagé avec le site et les vecteurs de test.
    pub fn code(&self) -> &'static str {
        match self {
            Refus::Format => "format",
            Refus::Alg => "alg",
            Refus::Typ => "typ",
            Refus::KidInconnu => "kid_inconnu",
            Refus::CleHorsValidite => "cle_hors_validite",
            Refus::Signature => "signature",
            Refus::Version => "version",
            Refus::Emetteur => "emetteur",
            Refus::Audience => "audience",
            Refus::Serveur => "serveur",
            Refus::PasEncoreValide => "pas_encore_valide",
            Refus::Expire => "expire",
            Refus::Duree => "duree",
            Refus::CleAppareil => "cle_appareil",
            Refus::Defi => "defi",
            Refus::RacineAbsente => "racine_absente",
            Refus::JeuDeClesSignature => "jeu_de_cles_signature",
            Refus::JeuDeClesFormat => "jeu_de_cles_format",
        }
    }
}

impl fmt::Display for Refus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Refus {}

/// Algorithme de la clé de l'appareil (`cnf.alg`).
///
/// Ed25519 partout où c'est possible ; P-256 en repli pour les navigateurs et
/// les Android dont WebCrypto ou le Keystore ne connaissent pas Ed25519.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlgCle {
    /// Clé publique de 32 octets, signature de 64 octets.
    #[serde(rename = "Ed25519")]
    Ed25519,
    /// Clé publique SEC1 non compressée de 65 octets (`04 || x || y`, ce que
    /// rend `exportKey("raw")` de WebCrypto), signature ECDSA SHA-256 au
    /// format brut `r || s` de 64 octets (ce que rend `sign` de WebCrypto).
    #[serde(rename = "P-256")]
    P256,
}

/// Lien entre le laissez-passer et la clé de l'appareil (`cnf`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub alg: AlgCle,
    /// Clé publique de l'appareil, base64url sans remplissage.
    pub pk: String,
}

/// La charge d'un laissez-passer.
///
/// Ni adresse électronique, ni adresse IP, ni nom d'appareil, ni droits sur la
/// bibliothèque : seulement de quoi dire qui, où, avec quelle clé, et jusqu'à
/// quand. Un champ inconnu est ignoré : c'est `v` qui gouverne le sens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revendications {
    pub iss: String,
    pub aud: String,
    /// Identifiant opaque du compte (jamais l'adresse électronique).
    pub sub: String,
    /// `server_id` du serveur, et de lui seul.
    pub sid: String,
    /// `device_id` tiré par le site.
    pub did: String,
    pub cnf: Confirmation,
    /// Le chemin direct (pair-à-pair) est-il permis ?
    pub p2p: bool,
    /// Clé publique Ed25519 du serveur, base64url, si le site la connaît.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srv_pk: Option<String>,
    pub iat: i64,
    pub nbf: i64,
    pub exp: i64,
    pub jti: String,
    pub v: u32,
}

/// Entête d'un laissez-passer. Champs inconnus refusés.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entete {
    alg: String,
    typ: String,
    kid: String,
}

/// Clé publique d'appareil décodée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleAppareil {
    pub alg: AlgCle,
    pub octets: Vec<u8>,
}

impl CleAppareil {
    /// Décode `cnf`. Refuse une clé de mauvaise taille ou de mauvaise forme.
    pub fn depuis_confirmation(cnf: &Confirmation) -> Result<Self, Refus> {
        let octets = b64url(&cnf.pk).ok_or(Refus::CleAppareil)?;
        let bonne_forme = match cnf.alg {
            AlgCle::Ed25519 => octets.len() == 32,
            AlgCle::P256 => octets.len() == 65 && octets[0] == 0x04,
        };
        if !bonne_forme {
            return Err(Refus::CleAppareil);
        }
        Ok(Self {
            alg: cnf.alg,
            octets,
        })
    }

    /// Vrai si `signature` (base64url) est une signature de `message` par
    /// cette clé.
    pub fn verifie(&self, message: &[u8], signature: &str) -> bool {
        let Some(sig) = b64url(signature) else {
            return false;
        };
        if sig.len() != 64 {
            return false;
        }
        let alg: &dyn ring::signature::VerificationAlgorithm = match self.alg {
            AlgCle::Ed25519 => &ring::signature::ED25519,
            AlgCle::P256 => &ring::signature::ECDSA_P256_SHA256_FIXED,
        };
        ring::signature::UnparsedPublicKey::new(alg, &self.octets)
            .verify(message, &sig)
            .is_ok()
    }
}

/// Un laissez-passer dont la signature et les champs ont été vérifiés.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaissezPasser {
    pub kid: String,
    pub revendications: Revendications,
    pub cle_appareil: CleAppareil,
}

impl LaissezPasser {
    /// Le laissez-passer est-il expiré à `maintenant`, tolérance comprise ?
    ///
    /// Vrai pour un laissez-passer accepté en mode secours
    /// ([`Contexte::grace_expiration`]).
    pub fn est_expire(&self, maintenant: i64) -> bool {
        maintenant > self.revendications.exp + TOLERANCE_HORLOGE_S
    }

    /// Vérifie la signature du défi de session par la clé de l'appareil.
    pub fn verifier_defi(&self, server_id: &str, defi: &str, signature: &str) -> Result<(), Refus> {
        let message = message_defi_session(server_id, defi);
        if self.cle_appareil.verifie(&message, signature) {
            Ok(())
        } else {
            Err(Refus::Defi)
        }
    }

    /// Vérifie la signature du défi d'une session DIRECTE (#6785) par la clé
    /// de l'appareil : message [`message_defi_direct`].
    pub fn verifier_defi_direct(
        &self,
        server_id: &str,
        defi: &str,
        signature: &str,
    ) -> Result<(), Refus> {
        let message = message_defi_direct(server_id, defi);
        if self.cle_appareil.verifie(&message, signature) {
            Ok(())
        } else {
            Err(Refus::Defi)
        }
    }
}

/// Ce contre quoi un laissez-passer est vérifié.
#[derive(Debug, Clone, Copy)]
pub struct Contexte<'a> {
    /// Le serveur visé : `sid` doit lui être égal.
    pub server_id: &'a str,
    /// Heure de vérification, en secondes Unix.
    pub maintenant: i64,
    /// Secondes pendant lesquelles un laissez-passer EXPIRÉ reste accepté.
    /// `0` hors panne du site ; la règle de secours du relais l'ouvre.
    pub grace_expiration: i64,
}

/// Une clé de signature du site, telle que publiée dans le jeu de clés.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleDuSite {
    pub kid: String,
    /// Toujours `EdDSA`.
    pub alg: String,
    /// Clé publique Ed25519, base64url sans remplissage.
    pub pk: String,
    /// Début de validité, secondes Unix.
    pub valable_du: i64,
    /// Fin de validité, secondes Unix.
    pub valable_au: i64,
}

/// Le document signé par la racine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentJeuDeCles {
    pub v: u32,
    /// Croissant : un relais n'accepte jamais un jeu plus ancien que le sien.
    pub version: u64,
    pub iss: String,
    pub cles: Vec<CleDuSite>,
}

/// Ce que publie `GET /api/v1/remote/keys` : le document exact, tel qu'il a
/// été signé, et le contenu du fichier `.minisig` de la racine.
///
/// Le document voyage en CHAÎNE : la signature porte sur ses octets exacts,
/// et aucune normalisation JSON n'a à être identique des deux côtés.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub jeu: String,
    pub minisig: String,
}

#[derive(Debug, Clone)]
struct CleDecodee {
    pk: Vec<u8>,
    valable_du: i64,
    valable_au: i64,
}

/// Jeu de clés du site, vérifié par la racine.
#[derive(Debug, Clone)]
pub struct JeuDeCles {
    version: u64,
    cles: Vec<(String, CleDecodee)>,
}

impl JeuDeCles {
    /// Vérifie la publication avec la clé racine (ligne base64 d'un `.pub`
    /// minisign), puis décode le jeu.
    ///
    /// Seules les signatures minisign « prédigérées » (BLAKE2b-512, l'unique
    /// mode de `minisign -S` actuel) sont acceptées.
    pub fn depuis_publication(publication: &Publication, racine: &str) -> Result<Self, Refus> {
        let racine = racine.trim();
        if racine.is_empty() {
            return Err(Refus::RacineAbsente);
        }
        let cle =
            minisign_verify::PublicKey::from_base64(racine).map_err(|_| Refus::RacineAbsente)?;
        let signature = minisign_verify::Signature::decode(&publication.minisig)
            .map_err(|_| Refus::JeuDeClesSignature)?;
        cle.verify(publication.jeu.as_bytes(), &signature, false)
            .map_err(|_| Refus::JeuDeClesSignature)?;

        let doc: DocumentJeuDeCles =
            serde_json::from_str(&publication.jeu).map_err(|_| Refus::JeuDeClesFormat)?;
        if doc.v != VERSION_FORMAT || doc.iss != EMETTEUR {
            return Err(Refus::JeuDeClesFormat);
        }
        let mut cles = Vec::with_capacity(doc.cles.len());
        for c in doc.cles {
            let pk = b64url(&c.pk).ok_or(Refus::JeuDeClesFormat)?;
            if c.alg != ALG || pk.len() != 32 || c.kid.is_empty() || c.valable_au < c.valable_du {
                return Err(Refus::JeuDeClesFormat);
            }
            if cles.iter().any(|(kid, _)| kid == &c.kid) {
                return Err(Refus::JeuDeClesFormat);
            }
            cles.push((
                c.kid,
                CleDecodee {
                    pk,
                    valable_du: c.valable_du,
                    valable_au: c.valable_au,
                },
            ));
        }
        Ok(Self {
            version: doc.version,
            cles,
        })
    }

    /// Version du jeu (champ `version`).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Ce `kid` est-il dans le jeu ?
    pub fn connait(&self, kid: &str) -> bool {
        self.cles.iter().any(|(k, _)| k == kid)
    }

    /// Lit l'entête sans rien vérifier d'autre : pour savoir si un `kid`
    /// inconnu justifie de relire le jeu de clés.
    pub fn kid_de(jeton: &str) -> Option<String> {
        let entete = jeton.split('.').next()?;
        let octets = b64url(entete)?;
        let e: Entete = serde_json::from_slice(&octets).ok()?;
        Some(e.kid)
    }

    /// Vérifie un laissez-passer, hors ligne.
    ///
    /// L'ordre compte : l'entête est jugée AVANT la signature (un `alg`
    /// étranger n'est jamais interprété), et la charge n'est lue qu'APRÈS
    /// (aucun champ d'un jeton non signé n'influence la décision).
    pub fn verifier(&self, jeton: &str, ctx: &Contexte<'_>) -> Result<LaissezPasser, Refus> {
        if jeton.len() > TAILLE_MAX {
            return Err(Refus::Format);
        }
        let mut parties = jeton.split('.');
        let (Some(e64), Some(c64), Some(s64), None) = (
            parties.next(),
            parties.next(),
            parties.next(),
            parties.next(),
        ) else {
            return Err(Refus::Format);
        };

        let entete_octets = b64url(e64).ok_or(Refus::Format)?;
        let entete: Entete = serde_json::from_slice(&entete_octets).map_err(|_| Refus::Format)?;
        if entete.alg != ALG {
            return Err(Refus::Alg);
        }
        if entete.typ != TYP {
            return Err(Refus::Typ);
        }
        let (_, cle) = self
            .cles
            .iter()
            .find(|(kid, _)| kid == &entete.kid)
            .ok_or(Refus::KidInconnu)?;

        let signature = b64url(s64).ok_or(Refus::Format)?;
        let signe = &jeton.as_bytes()[..e64.len() + 1 + c64.len()];
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &cle.pk)
            .verify(signe, &signature)
            .map_err(|_| Refus::Signature)?;

        let charge = b64url(c64).ok_or(Refus::Format)?;
        let r: Revendications = serde_json::from_slice(&charge).map_err(|_| Refus::Format)?;

        if r.v != VERSION_FORMAT {
            return Err(Refus::Version);
        }
        if r.iss != EMETTEUR {
            return Err(Refus::Emetteur);
        }
        if r.aud != AUDIENCE {
            return Err(Refus::Audience);
        }
        if r.sid != ctx.server_id {
            return Err(Refus::Serveur);
        }
        if r.sub.is_empty() || r.did.is_empty() || r.jti.is_empty() {
            return Err(Refus::Format);
        }
        let duree = r.exp - r.iat;
        if !(0..=DUREE_MAX_S).contains(&duree) || r.nbf > r.exp {
            return Err(Refus::Duree);
        }
        let t = ctx.maintenant;
        if r.nbf > t + TOLERANCE_HORLOGE_S || r.iat > t + TOLERANCE_HORLOGE_S {
            return Err(Refus::PasEncoreValide);
        }
        if t > r.exp + TOLERANCE_HORLOGE_S + ctx.grace_expiration.max(0) {
            return Err(Refus::Expire);
        }
        // La clé du site doit avoir été valable quand elle a signé, et l'être
        // encore : une clé retirée du jeu ou arrivée à terme ne signe plus.
        if r.iat < cle.valable_du - TOLERANCE_HORLOGE_S || t > cle.valable_au + TOLERANCE_HORLOGE_S
        {
            return Err(Refus::CleHorsValidite);
        }
        let cle_appareil = CleAppareil::depuis_confirmation(&r.cnf)?;
        if let Some(srv) = &r.srv_pk
            && b64url(srv).is_none_or(|o| o.len() != 32)
        {
            return Err(Refus::Format);
        }

        Ok(LaissezPasser {
            kid: entete.kid,
            revendications: r,
            cle_appareil,
        })
    }
}

/// Le message que l'appareil signe pour ouvrir (ou renouveler) une session :
/// `tune-remote/session/v1 \n server_id \n défi`, en UTF-8.
///
/// Le domaine en tête empêche qu'une signature faite pour un autre usage
/// (enregistrement d'un appareil sur le site, défi du serveur) serve ici.
pub fn message_defi_session(server_id: &str, defi: &str) -> Vec<u8> {
    format!("{DOMAINE_DEFI_SESSION}\n{server_id}\n{defi}").into_bytes()
}

/// Le message que l'appareil signe pour ouvrir (ou renouveler) une session
/// DIRECTE sur le serveur (#6785) :
/// `tune-remote/direct/v1 \n server_id \n défi`, en UTF-8. Le défi est tiré
/// par le serveur, sur la connexion même, et ne sert qu'une fois.
pub fn message_defi_direct(server_id: &str, defi: &str) -> Vec<u8> {
    format!("{DOMAINE_DEFI_DIRECT}\n{server_id}\n{defi}").into_bytes()
}

/// Le message que le serveur signe pour une requête au site (#6785) :
///
/// ```text
/// tune-remote/requete-serveur/v1
/// <MÉTHODE>
/// <chemin, sans l'hôte, requête comprise>
/// <server_id>
/// <horodatage, secondes Unix>
/// <nonce>
/// <SHA-256 du corps exact, base64url sans remplissage>
/// ```
///
/// Lignes séparées par `\n`, sans `\n` final. Le site vérifie la signature
/// avec la clé inscrite pour ce `server_id`, refuse un horodatage à plus de
/// [`TOLERANCE_REQUETE_SERVEUR_S`] de son horloge et un nonce déjà vu dans
/// cette fenêtre.
pub fn message_requete_serveur(
    methode: &str,
    chemin: &str,
    server_id: &str,
    horodatage: i64,
    nonce: &str,
    corps: &[u8],
) -> Vec<u8> {
    let empreinte = ring::digest::digest(&ring::digest::SHA256, corps);
    format!(
        "{DOMAINE_REQUETE_SERVEUR}\n{methode}\n{chemin}\n{server_id}\n{horodatage}\n{nonce}\n{}",
        URL_SAFE_NO_PAD.encode(empreinte.as_ref())
    )
    .into_bytes()
}

/// Vérifie la signature d'une requête du serveur au site (la règle que le
/// site applique, rejouée ici pour les tests et pour le contrat).
#[allow(clippy::too_many_arguments)]
pub fn verifier_requete_serveur(
    cle_serveur: &str,
    methode: &str,
    chemin: &str,
    server_id: &str,
    horodatage: i64,
    nonce: &str,
    corps: &[u8],
    signature: &str,
) -> Result<(), Refus> {
    let octets = b64url(cle_serveur).ok_or(Refus::CleAppareil)?;
    if octets.len() != 32 {
        return Err(Refus::CleAppareil);
    }
    let cle = CleAppareil {
        alg: AlgCle::Ed25519,
        octets,
    };
    let message = message_requete_serveur(methode, chemin, server_id, horodatage, nonce, corps);
    if cle.verifie(&message, signature) {
        Ok(())
    } else {
        Err(Refus::Defi)
    }
}

/// Le message que le serveur signe pour s'enregistrer sur le relais :
/// `tune-remote/serveur/v1 \n server_id \n défi`.
pub fn message_defi_serveur(server_id: &str, defi: &str) -> Vec<u8> {
    format!("{DOMAINE_DEFI_SERVEUR}\n{server_id}\n{defi}").into_bytes()
}

/// Vérifie la signature d'un défi d'enregistrement par la clé du serveur
/// (Ed25519, base64url).
pub fn verifier_defi_serveur(
    cle_serveur: &str,
    server_id: &str,
    defi: &str,
    signature: &str,
) -> Result<(), Refus> {
    let octets = b64url(cle_serveur).ok_or(Refus::CleAppareil)?;
    if octets.len() != 32 {
        return Err(Refus::CleAppareil);
    }
    let cle = CleAppareil {
        alg: AlgCle::Ed25519,
        octets,
    };
    if cle.verifie(&message_defi_serveur(server_id, defi), signature) {
        Ok(())
    } else {
        Err(Refus::Defi)
    }
}

/// Le message que le serveur signe pour inscrire sa clé publique sur le
/// compte : `tune-remote/inscription/v1 \n server_id \n clé publique`. Le
/// site prouve ainsi que le serveur détient la clé qu'il inscrit.
pub fn message_inscription_serveur(server_id: &str, cle_publique: &str) -> Vec<u8> {
    format!("{DOMAINE_INSCRIPTION_SERVEUR}\n{server_id}\n{cle_publique}").into_bytes()
}

/// Vérifie la preuve d'une inscription de clé de serveur.
pub fn verifier_inscription_serveur(
    cle_serveur: &str,
    server_id: &str,
    preuve: &str,
) -> Result<(), Refus> {
    let octets = b64url(cle_serveur).ok_or(Refus::CleAppareil)?;
    if octets.len() != 32 {
        return Err(Refus::CleAppareil);
    }
    let cle = CleAppareil {
        alg: AlgCle::Ed25519,
        octets,
    };
    if cle.verifie(&message_inscription_serveur(server_id, cle_serveur), preuve) {
        Ok(())
    } else {
        Err(Refus::Defi)
    }
}

/// Clé Ed25519 du serveur : la paire qui l'identifie auprès du relais et du
/// site (et, demain, son identité d'endpoint pair-à-pair).
pub struct CleServeur {
    paire: ring::signature::Ed25519KeyPair,
}

impl CleServeur {
    /// Tire une graine de 32 octets au générateur du système.
    pub fn nouvelle_graine() -> Result<[u8; 32], Refus> {
        use ring::rand::SecureRandom;
        let mut graine = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut graine)
            .map_err(|_| Refus::CleAppareil)?;
        Ok(graine)
    }

    /// La paire dérivée d'une graine.
    pub fn depuis_graine(graine: &[u8]) -> Result<Self, Refus> {
        if graine.len() != 32 {
            return Err(Refus::CleAppareil);
        }
        let paire = ring::signature::Ed25519KeyPair::from_seed_unchecked(graine)
            .map_err(|_| Refus::CleAppareil)?;
        Ok(Self { paire })
    }

    /// Clé publique, base64url sans remplissage.
    pub fn cle_publique(&self) -> String {
        use ring::signature::KeyPair;
        URL_SAFE_NO_PAD.encode(self.paire.public_key().as_ref())
    }

    /// Signe le défi d'enregistrement du relais ; base64url.
    pub fn signer_defi(&self, server_id: &str, defi: &str) -> String {
        self.signer(&message_defi_serveur(server_id, defi))
    }

    /// Signe un message quelconque ; base64url.
    pub fn signer(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.paire.sign(message).as_ref())
    }
}

/// Un jeton aléatoire de 256 bits, base64url : jeton de session, défi.
///
/// `None` si le générateur du système fait défaut — l'appelant refuse alors
/// d'ouvrir quoi que ce soit plutôt que de tirer un jeton faible.
pub fn jeton_aleatoire() -> Option<String> {
    use ring::rand::SecureRandom;
    let mut octets = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut octets).ok()?;
    Some(URL_SAFE_NO_PAD.encode(octets))
}

/// Décodage base64url STRICT : sans remplissage, sans bits de fin, sans
/// caractère hors alphabet.
pub fn b64url(texte: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(texte).ok()
}

/// Encodage base64url sans remplissage.
pub fn en_b64url(octets: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(octets)
}

/// Encodage base64 standard (celui de minisign).
pub fn en_b64(octets: &[u8]) -> String {
    STANDARD.encode(octets)
}

#[cfg(test)]
mod tests;
