//! La poignée de main Noise `KKpsk2`, côté SERVEUR.
//!
//! Deux points de la spécification commandent tout ce fichier :
//!
//! 1. **Le serveur est l'initiateur Noise**, l'enceinte le répondeur, *quel que
//!    soit le sens TCP*. Que ce soit l'enceinte qui ait composé vers nous n'y
//!    change rien : c'est Tune qui écrit le message 1.
//! 2. **Le prologue est fait des octets exacts des deux messages en clair**,
//!    dans l'ordre `client/init` puis `server/init`. D'où la règle tenue ici :
//!    le texte reçu est conservé tel quel, le texte émis est sérialisé UNE
//!    fois et gardé — jamais reconstruit.
//!
//! La cryptographie n'est pas écrite ici : elle est déléguée à la caisse
//! `snow`, implémentation pur Rust du cadre Noise. Ce fichier n'est que la
//! machine à états qui l'alimente et le cadrage JSON autour.
//!
//! ## Ce qui n'est pas prouvé à ce stade
//!
//! S2-a n'emploie que la PSK **Sentinelle**, qui est publique. La confiance
//! qu'on peut accorder au pair après cette poignée de main est donc celle d'un
//! canal chiffré, **pas** celle d'un pair authentifié : n'importe qui sur le
//! réseau peut la mener à bien. C'est S2-b, avec les PSK `lt` et `pr`, qui
//! transformera ce tuyau en preuve d'identité. Rien dans ce module ne doit
//! laisser croire l'inverse.

use snow::{Builder, HandshakeState};

use super::identite::{Identite, b64url, cle_publique_du_pair, depuis_b64url};
use super::messages::{
    ChargeMessageUn, ClientInit, Enveloppe, EnveloppeBrute, NoiseHandshake, ServerInit,
    TYPE_CLIENT_INIT, TYPE_NOISE_HANDSHAKE, TYPE_SERVER_INIT,
};
use super::psk::TAILLE_PSK;
use super::suite::Suite;
use super::transport::{MAX_MESSAGE_NOISE, TransportNoise};
use super::{ErreurSendspin, VERSION_PROTOCOLE};

/// Ce qu'on sait du pair une fois la poignée de main terminée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfosPair {
    /// La clé publique de l'enceinte, base64url — son identité durable.
    pub client_id: String,
    /// La suite que l'enceinte a choisie.
    pub suite: Suite,
    /// L'identifiant de la PSK qui a admis la connexion.
    pub psk_id: String,
}

/// La machine à états de la poignée de main, côté serveur.
///
/// Les étapes sont enchaînées par le type : [`Self::accueillir`] rend l'objet,
/// [`Self::message_un`] le fait avancer, [`Self::message_deux`] le CONSOMME
/// pour rendre le transport. On ne peut donc pas réutiliser une poignée de main
/// terminée ni sauter une étape.
pub struct PoigneeServeur {
    suite: Suite,
    client_id: String,
    psk_id: String,
    server_init_texte: String,
    etat: HandshakeState,
    message_un_envoye: bool,
}

impl std::fmt::Debug for PoigneeServeur {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoigneeServeur")
            .field("client_id", &self.client_id)
            .field("suite", &self.suite)
            .field("psk_id", &self.psk_id)
            .field("message_un_envoye", &self.message_un_envoye)
            .finish()
    }
}

impl PoigneeServeur {
    /// Étape 1 — lit le `client/init` **tel qu'il est arrivé** et prépare le
    /// `server/init`.
    ///
    /// `client_init_texte` doit être la chaîne reçue, octet pour octet. La
    /// re-sérialiser avant de la passer ici casserait le prologue.
    ///
    /// Le `server/init` n'est pas envoyé par cette fonction : elle le rend par
    /// [`Self::server_init_texte`], et c'est l'appelant qui l'écrit sur le fil.
    /// Il doit envoyer **exactement** cette chaîne.
    pub fn accueillir(
        identite: &Identite,
        client_init_texte: &str,
        psk: &[u8; TAILLE_PSK],
    ) -> Result<Self, ErreurSendspin> {
        let brute: EnveloppeBrute = serde_json::from_str(client_init_texte)
            .map_err(|e| ErreurSendspin::MessageIllisible(format!("client/init : {e}")))?;
        if brute.type_message != TYPE_CLIENT_INIT {
            return Err(ErreurSendspin::MessageIllisible(format!(
                "{TYPE_CLIENT_INIT} attendu, {} recu",
                brute.type_message
            )));
        }
        let init: ClientInit = serde_json::from_value(brute.payload)
            .map_err(|e| ErreurSendspin::MessageIllisible(format!("charge client/init : {e}")))?;

        if init.version != VERSION_PROTOCOLE {
            return Err(ErreurSendspin::VersionInconnue(init.version));
        }
        let suite = Suite::depuis_nom(&init.suite)?;
        let client_public = cle_publique_du_pair(&init.client_id)?;

        // Le server/init est serialise UNE fois. C'est cette chaine-la qui
        // entre dans le prologue ET qui part sur le fil ; les deux ne peuvent
        // donc pas diverger.
        let server_init_texte = serde_json::to_string(&Enveloppe::nouvelle(
            TYPE_SERVER_INIT,
            ServerInit {
                server_id: identite.id(),
                version: VERSION_PROTOCOLE,
            },
        ))
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("server/init : {e}")))?;

        let mut prologue = Vec::with_capacity(client_init_texte.len() + server_init_texte.len());
        prologue.extend_from_slice(client_init_texte.as_bytes());
        prologue.extend_from_slice(server_init_texte.as_bytes());

        let motif = suite.motif_noise();
        let params = motif
            .parse()
            .map_err(|e| ErreurSendspin::Noise(format!("motif {motif} : {e:?}")))?;

        // Tune est l'INITIATEUR, meme si c'est l'enceinte qui a compose.
        let etat = Builder::new(params)
            .local_private_key(identite.prive())
            .map_err(|e| ErreurSendspin::Noise(format!("cle locale : {e}")))?
            .remote_public_key(&client_public)
            .map_err(|e| ErreurSendspin::Noise(format!("cle du pair : {e}")))?
            .prologue(&prologue)
            .map_err(|e| ErreurSendspin::Noise(format!("prologue : {e}")))?
            .psk(Suite::position_psk(), psk)
            .map_err(|e| ErreurSendspin::Noise(format!("psk : {e}")))?
            .build_initiator()
            .map_err(|e| ErreurSendspin::Noise(format!("initiateur : {e}")))?;

        Ok(Self {
            suite,
            client_id: init.client_id,
            psk_id: super::psk::identifiant(psk),
            server_init_texte,
            etat,
            message_un_envoye: false,
        })
    }

    /// Le texte `server/init` à écrire sur le fil, octet pour octet.
    pub fn server_init_texte(&self) -> &str {
        &self.server_init_texte
    }

    /// La suite choisie par l'enceinte.
    pub fn suite(&self) -> Suite {
        self.suite
    }

    /// L'identifiant de l'enceinte, connu dès le `client/init`.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Étape 2 — le premier `noise/handshake`.
    ///
    /// Sa charge utile chiffrée porte le `psk_id` : le motif `KKpsk2` ne mêle
    /// la PSK qu'au SECOND message, donc l'enceinte peut lire celui-ci sans
    /// encore savoir quelle PSK employer — et y apprend justement laquelle.
    pub fn message_un(&mut self) -> Result<String, ErreurSendspin> {
        if self.message_un_envoye {
            return Err(ErreurSendspin::EtatInattendu("message noise 1 deja emis"));
        }
        let charge = serde_json::to_vec(&ChargeMessageUn {
            psk_id: self.psk_id.clone(),
        })
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("charge du message 1 : {e}")))?;

        let mut tampon = vec![0u8; MAX_MESSAGE_NOISE];
        let n = self
            .etat
            .write_message(&charge, &mut tampon)
            .map_err(|e| ErreurSendspin::Noise(format!("message 1 : {e}")))?;
        tampon.truncate(n);
        self.message_un_envoye = true;

        serde_json::to_string(&Enveloppe::nouvelle(
            TYPE_NOISE_HANDSHAKE,
            NoiseHandshake {
                data: b64url(&tampon),
            },
        ))
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("noise/handshake : {e}")))
    }

    /// Étape 3 — lit le second `noise/handshake` et bascule en transport.
    ///
    /// Consomme la poignée de main : au-delà, il n'y a plus que le tuyau.
    pub fn message_deux(
        mut self,
        texte: &str,
    ) -> Result<(TransportNoise, InfosPair), ErreurSendspin> {
        if !self.message_un_envoye {
            return Err(ErreurSendspin::EtatInattendu(
                "message noise 2 lu avant d'avoir emis le 1",
            ));
        }
        let brute: EnveloppeBrute = serde_json::from_str(texte)
            .map_err(|e| ErreurSendspin::MessageIllisible(format!("noise/handshake : {e}")))?;
        if brute.type_message != TYPE_NOISE_HANDSHAKE {
            return Err(ErreurSendspin::MessageIllisible(format!(
                "{TYPE_NOISE_HANDSHAKE} attendu, {} recu",
                brute.type_message
            )));
        }
        let poignee: NoiseHandshake = serde_json::from_value(brute.payload).map_err(|e| {
            ErreurSendspin::MessageIllisible(format!("charge noise/handshake : {e}"))
        })?;
        let brut = depuis_b64url(&poignee.data)?;

        let mut tampon = vec![0u8; MAX_MESSAGE_NOISE];
        self.etat
            .read_message(&brut, &mut tampon)
            .map_err(|e| ErreurSendspin::Noise(format!("message 2 : {e}")))?;

        if !self.etat.is_handshake_finished() {
            return Err(ErreurSendspin::Noise(
                "la poignee de main n'est pas terminee apres le message 2".into(),
            ));
        }
        let transport = self
            .etat
            .into_transport_mode()
            .map_err(|e| ErreurSendspin::Noise(format!("bascule en transport : {e}")))?;

        Ok((
            TransportNoise::nouveau(transport),
            InfosPair {
                client_id: self.client_id,
                suite: self.suite,
                psk_id: self.psk_id,
            },
        ))
    }
}

/// Le pendant répondeur, **réservé aux tests**.
///
/// Tune n'est jamais le répondeur Noise : c'est le rôle de l'enceinte. Ce
/// module n'existe donc que pour exercer notre serveur en mémoire, sans
/// réseau ni matériel. Il est derrière `#[cfg(test)]` volontairement — il ne
/// doit pas devenir un chemin de production par inadvertance, et surtout il ne
/// doit jamais être confondu avec une preuve d'interopérabilité : un serveur
/// qui ne réussit la poignée de main que contre son propre miroir n'a rien
/// prouvé. La preuve, elle, se fait contre une implémentation tierce.
#[cfg(test)]
pub(crate) mod repondeur_de_test {
    use super::*;

    pub struct RepondeurDeTest {
        etat: HandshakeState,
    }

    impl RepondeurDeTest {
        pub fn nouveau(
            identite: &Identite,
            serveur_public: &[u8; 32],
            client_init_texte: &str,
            server_init_texte: &str,
            suite: Suite,
            psk: &[u8; TAILLE_PSK],
        ) -> Result<Self, ErreurSendspin> {
            let mut prologue =
                Vec::with_capacity(client_init_texte.len() + server_init_texte.len());
            prologue.extend_from_slice(client_init_texte.as_bytes());
            prologue.extend_from_slice(server_init_texte.as_bytes());

            let motif = suite.motif_noise();
            let params = motif
                .parse()
                .map_err(|e| ErreurSendspin::Noise(format!("motif : {e:?}")))?;

            let etat = Builder::new(params)
                .local_private_key(identite.prive())
                .map_err(|e| ErreurSendspin::Noise(format!("cle locale : {e}")))?
                .remote_public_key(serveur_public)
                .map_err(|e| ErreurSendspin::Noise(format!("cle du pair : {e}")))?
                .prologue(&prologue)
                .map_err(|e| ErreurSendspin::Noise(format!("prologue : {e}")))?
                .psk(Suite::position_psk(), psk)
                .map_err(|e| ErreurSendspin::Noise(format!("psk : {e}")))?
                .build_responder()
                .map_err(|e| ErreurSendspin::Noise(format!("repondeur : {e}")))?;

            Ok(Self { etat })
        }

        /// Lit le message 1 et rend le `psk_id` que le serveur y a glissé.
        pub fn lire_message_un(&mut self, texte: &str) -> Result<String, ErreurSendspin> {
            let brute: EnveloppeBrute = serde_json::from_str(texte)
                .map_err(|e| ErreurSendspin::MessageIllisible(format!("{e}")))?;
            let poignee: NoiseHandshake = serde_json::from_value(brute.payload)
                .map_err(|e| ErreurSendspin::MessageIllisible(format!("{e}")))?;
            let brut = depuis_b64url(&poignee.data)?;

            let mut tampon = vec![0u8; MAX_MESSAGE_NOISE];
            let n = self
                .etat
                .read_message(&brut, &mut tampon)
                .map_err(|e| ErreurSendspin::Noise(format!("lecture message 1 : {e}")))?;
            tampon.truncate(n);
            let charge: ChargeMessageUn = serde_json::from_slice(&tampon)
                .map_err(|e| ErreurSendspin::MessageIllisible(format!("psk_id : {e}")))?;
            Ok(charge.psk_id)
        }

        pub fn ecrire_message_deux(&mut self) -> Result<String, ErreurSendspin> {
            let mut tampon = vec![0u8; MAX_MESSAGE_NOISE];
            let n = self
                .etat
                .write_message(&[], &mut tampon)
                .map_err(|e| ErreurSendspin::Noise(format!("message 2 : {e}")))?;
            tampon.truncate(n);
            serde_json::to_string(&Enveloppe::nouvelle(
                TYPE_NOISE_HANDSHAKE,
                NoiseHandshake {
                    data: b64url(&tampon),
                },
            ))
            .map_err(|e| ErreurSendspin::MessageIllisible(format!("{e}")))
        }

        pub fn en_transport(self) -> Result<TransportNoise, ErreurSendspin> {
            let transport = self
                .etat
                .into_transport_mode()
                .map_err(|e| ErreurSendspin::Noise(format!("transport : {e}")))?;
            Ok(TransportNoise::nouveau(transport))
        }
    }
}
