//! Sendspin — le rôle SERVEUR du protocole (#3326, phase 2, briques S2-a/S2-b).
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
//! S2-b en cours : identite et PSK longue duree sont conservees dans le
//! magasin prive. Le transport sait lier les cles aux pairs et mener un
//! reechange. CPace et le wrapping des codes sont eprouves dans pake.
//! Les trois parcours d'appairage et leur interaction operateur restent a
//! brancher. La Sentinelle publique n'authentifie pas un pair.
//!
//! Non livre :
//! - l'appairage complet (PSK provisoire, CPace, codes) — S2-b ;
//! - le **son** : horloge, cadrage, encodeur Opus, `stream/start` — S2-c ;
//! - la **synchronisation** à plusieurs enceintes — S2-d ;
//! - le branchement d'`OutputTarget` : deux décisions de produit ne sont pas
//!   tranchées (voir la note de fin de ce document) et appartiennent à
//!   Bertrand. Aucune zone Sendspin ne peut naître de ce module.
//!
//! ## D'où vient le contrat de fil
//!
//! De notre note de lecture, `docs/sendspin-protocole.md`, et de la revision
//! `Sendspin/spec@8a8b1cbd6764ea116dcaa07e41544a97bc13080c` epinglee pour S2-b.
//! Cette revision porte `Community-Spec-1.0`. Aucune caisse Sendspin n'est
//! au manifeste ; la reference tierce sert aux tests d'interoperabilite.
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
//!
//! ## Le mode de transition (ajouté le 11/09/2026)
//!
//! Au releve du 11/09/2026, le lecteur publie ne parlait pas le chiffre : `aiosendspin`
//! 6.0.5 n'embarque aucun module `noise/` et ne connaît ni `client/init` ni
//! `server/activate`. Tune, qui n'implémentait que la branche chiffrée, était
//! conforme et incapable de parler à une enceinte installée.
//!
//! [`transition::ModeTransition`] ouvre une porte **supplémentaire** : un
//! `client/hello` en clair comme tout premier message. Elle est **fermée par
//! défaut**, elle ne remplace jamais la branche chiffrée — l'aiguillage se fait
//! sur le TYPE du premier message, pas sur un échec — et une session qui
//! l'emprunte est nommée comme telle dans le journal et au registre.

pub mod appairage;
pub mod identite;
pub mod jeton;
pub mod magasin;
pub mod messages;
pub mod pake;
pub mod poignee;
pub mod psk;
pub mod registre;
pub mod suite;
pub mod transition;
pub mod transport;

pub use identite::Identite;
pub use poignee::PoigneeServeur;
pub use registre::PairVu;
pub use suite::Suite;
pub use transition::ModeTransition;
pub use transport::TransportNoise;

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
    /// Un pair a ouvert par un `client/hello` en clair alors que le mode de
    /// transition n'est pas armé. Ce n'est pas une panne : c'est le refus
    /// attendu, et le défaut.
    ClairRefuse,
    /// Un pair a prétendu **en clair** à un `client_id` que nous avons déjà vu
    /// mener une poignée de main Noise. Refus de rétrogradation : ce pair sait
    /// se connecter chiffré, rien ne justifie qu'il retombe en clair — et le
    /// `client_id` d'une session en clair n'est qu'une prétention.
    RetrogradationRefusee(String),
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
            Self::ClairRefuse => write!(
                f,
                "client/hello en clair refusé : le mode de transition n'est pas armé \
                 ({})",
                transition::VARIABLE_ENVIRONNEMENT
            ),
            Self::RetrogradationRefusee(id) => write!(
                f,
                "rétrogradation refusée : {id} a déjà mené une poignée de main Noise, \
                 il ne peut pas revenir en clair"
            ),
        }
    }
}

impl std::error::Error for ErreurSendspin {}
