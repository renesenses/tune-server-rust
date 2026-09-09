//! Sendspin — le rôle SERVEUR du protocole (#3326, phase 2, brique S2-a).
//!
//! Rappel du renversement de vocabulaire posé en phase 1 et vérifié depuis :
//! dans Sendspin **le « client » est l'enceinte** et **le « serveur » est la
//! source de musique**. Pour envoyer du son vers une enceinte, Tune doit donc
//! implémenter le rôle SERVEUR — le plus chargé des deux. Ce module ne contient
//! que ce rôle-là.
//!
//! ## Ce que S2-a livre, et ce qu'elle ne livre PAS
//!
//! Livré : le tuyau chiffré. L'annonce mDNS `_sendspin-server._tcp.local.`
//! (mode client-initié, que la spécification impose au même titre que le
//! parcours), le point d'accès WebSocket `ws://`, la poignée de main Noise
//! `KKpsk2` dans les **deux** suites imposées au serveur, et la séquence
//! `client/init` → `server/init` → `noise/handshake` ×2 → `server/hello` →
//! `client/hello` → `server/activate`.
//!
//! Non livré, volontairement, et chacun dans sa brique :
//! - l'**appairage** (PSK `lt` / `pr`, code d'appairage CPace, clip audio de
//!   chiffres) — S2-b. Ici, seule la PSK **Sentinelle** est employée : c'est
//!   une constante publiée, elle n'authentifie personne, et c'est exactement
//!   son rôle dans la spécification (« used when no other PSK applies ») ;
//! - le **son** : horloge, cadrage, encodeur Opus, `stream/start` — S2-c ;
//! - la **synchronisation** à plusieurs enceintes — S2-d ;
//! - le branchement d'`OutputTarget` : deux décisions de produit ne sont pas
//!   tranchées (voir la note de fin de ce document) et appartiennent à
//!   Bertrand. Aucune zone Sendspin ne peut naître de ce module.
//!
//! ## D'où vient le contrat de fil
//!
//! De notre propre note de lecture, `docs/sendspin-protocole.md`. Le dépôt de
//! spécification ne porte AUCUNE licence : rien ne nous autorise à en recopier
//! le texte, et rien ici ne le fait. Aucune caisse Sendspin n'est au manifeste.
//!
//! ## Le piège d'interopérabilité à ne jamais perdre de vue
//!
//! Le **prologue** Noise est la concaténation des OCTETS EXACTS des deux
//! messages en clair tels qu'ils ont circulé : le texte `client/init` reçu,
//! suivi du texte `server/init` envoyé. Re-sérialiser l'un des deux (ordre des
//! clés, espaces) donne un prologue différent, et la poignée de main échoue au
//! second message avec une erreur de déchiffrement qui ne nomme pas la cause.
//! C'est pourquoi [`poignee::PoigneeServeur`] conserve les deux textes tels
//! quels et ne les reconstruit jamais. Le témoin
//! `un_prologue_reconstruit_fait_echouer_la_poignee` garde ce point.

pub mod identite;
pub mod messages;
pub mod poignee;
pub mod psk;
pub mod registre;
pub mod suite;
pub mod transport;

pub use identite::Identite;
pub use poignee::PoigneeServeur;
pub use registre::PairVu;
pub use suite::Suite;
pub use transport::TransportNoise;

/// L'identité Sendspin de ce serveur, pour la durée du processus.
///
/// **Elle n'est pas persistée**, et c'est délibéré : la clé de longue durée du
/// serveur n'a de sens qu'avec l'appairage, qui est S2-b — c'est là qu'une
/// enceinte appairée doit survivre à un redémarrage. Tant que seule la PSK
/// Sentinelle est employée, un `server_id` qui change à chaque relance ne casse
/// rien, puisque rien ne s'y était lié.
///
/// Le jour où S2-b arrivera, c'est cette fonction qu'il faudra faire lire un
/// fichier — et pas ajouter une seconde source d'identité à côté.
pub fn identite_du_serveur() -> &'static Identite {
    static IDENTITE: std::sync::OnceLock<Identite> = std::sync::OnceLock::new();
    IDENTITE.get_or_init(Identite::generer)
}

/// Version du cœur du protocole. La spécification la fixe à `1`, et les deux
/// messages en clair la portent ; un pair qui annonce autre chose n'est pas
/// admis, faute de savoir ce qu'il attend.
pub const VERSION_PROTOCOLE: u32 = 1;

/// Chemin du point d'accès WebSocket que Tune annonce dans le TXT `path`.
///
/// La spécification rend le TXT `path` REQUIS et ne définit aucun défaut : un
/// pair qui ne le lit pas n'a pas le droit d'en inventer un. Cette constante
/// est donc la valeur que Tune **annonce**, jamais un repli à la lecture — la
/// phase 1 tient déjà la même ligne côté parcours
/// (`discovery::sendspin::CHEMIN_RECOMMANDE`).
pub const CHEMIN_POINT_D_ACCES: &str = "/sendspin";

/// Ce qui peut rater dans la poignée de main.
///
/// Un seul type, volontairement : la spécification n'a **aucun message
/// d'erreur applicatif**, et la seule réaction admise à un échec de poignée de
/// main est de fermer le WebSocket sans rien dire au pair. Ces variantes ne
/// servent donc qu'à NOUS — au journal et aux tests — et ne partent jamais sur
/// le fil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurSendspin {
    /// Le JSON en clair est illisible, ou son champ `type` n'est pas celui
    /// attendu à ce point de la séquence.
    MessageIllisible(String),
    /// Le pair annonce une version du cœur du protocole que nous ne savons pas
    /// parler.
    VersionInconnue(u32),
    /// Le pair a choisi une suite que la spécification ne définit pas. Un
    /// serveur doit supporter les DEUX suites, donc ce refus ne peut venir que
    /// d'un nom hors spécification.
    SuiteInconnue(String),
    /// Un identifiant de pair n'est pas une clé publique X25519 base64url de
    /// 43 caractères.
    IdentifiantInvalide(String),
    /// La couche Noise a refusé : clés qui ne correspondent pas, PSK fausse,
    /// prologue différent, ou message tronqué. Noise ne dit pas laquelle.
    Noise(String),
    /// La séquence a été jouée dans le désordre par notre propre code.
    EtatInattendu(&'static str),
}

impl std::fmt::Display for ErreurSendspin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MessageIllisible(d) => write!(f, "message sendspin illisible : {d}"),
            Self::VersionInconnue(v) => {
                write!(
                    f,
                    "version de protocole {v} inconnue (attendu {VERSION_PROTOCOLE})"
                )
            }
            Self::SuiteInconnue(s) => write!(f, "suite noise inconnue : {s}"),
            Self::IdentifiantInvalide(d) => write!(f, "identifiant sendspin invalide : {d}"),
            Self::Noise(d) => write!(f, "couche noise : {d}"),
            Self::EtatInattendu(d) => write!(f, "séquence sendspin hors d'ordre : {d}"),
        }
    }
}

impl std::error::Error for ErreurSendspin {}
