//! Les messages de la séquence d'établissement, et rien d'autre.
//!
//! Trois messages circulent **en clair**, en trames WebSocket texte :
//! `client/init`, `server/init`, `noise/handshake`. Tout ce qui suit passe en
//! trames binaires, chiffré par Noise, l'octet 0 du déchiffré portant le type.
//!
//! Les messages de flux (`stream/*`), d'horloge (`client/time`, `server/time`)
//! et d'appairage (`client/pair-*`) ne sont PAS ici : ils appartiennent à S2-b
//! et S2-c.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// L'enveloppe commune : un `type` et une charge utile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enveloppe<T> {
    #[serde(rename = "type")]
    pub type_message: String,
    pub payload: T,
}

impl<T> Enveloppe<T> {
    pub fn nouvelle(type_message: &str, payload: T) -> Self {
        Self {
            type_message: type_message.to_string(),
            payload,
        }
    }
}

/// Ce qu'on lit d'une trame texte avant de savoir de quel message il s'agit.
///
/// Le `payload` reste brut : la séquence impose un type précis à chaque étape,
/// et lire la charge utile avant d'avoir vérifié le type reviendrait à accepter
/// un `noise/handshake` là où un `client/init` est attendu.
#[derive(Debug, Clone, Deserialize)]
pub struct EnveloppeBrute {
    #[serde(rename = "type")]
    pub type_message: String,
    #[serde(default)]
    pub payload: Value,
}

pub const TYPE_CLIENT_INIT: &str = "client/init";
pub const TYPE_SERVER_INIT: &str = "server/init";
pub const TYPE_NOISE_HANDSHAKE: &str = "noise/handshake";
pub const TYPE_SERVER_HELLO: &str = "server/hello";
pub const TYPE_CLIENT_HELLO: &str = "client/hello";
pub const TYPE_SERVER_ACTIVATE: &str = "server/activate";
pub const TYPE_CLIENT_GOODBYE: &str = "client/goodbye";

/// `client/init` — l'enceinte ouvre la conversation et **choisit** la suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInit {
    /// Clé publique X25519 de l'enceinte, base64url, 43 caractères.
    pub client_id: String,
    pub version: u32,
    /// Le nom de la suite choisie. Aucune négociation : le serveur suit.
    pub suite: String,
}

/// `server/init` — Tune répond par son identité.
///
/// Pas de champ `suite` : le choix a déjà été fait par le client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInit {
    pub server_id: String,
    pub version: u32,
}

/// `noise/handshake` — porte un message Noise, base64url sans remplissage.
///
/// Le même type sert aux deux messages de la poignée de main ; c'est la
/// position dans la séquence qui les distingue, pas le contenu de l'enveloppe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseHandshake {
    pub data: String,
}

/// La charge utile chiffrée DANS le message Noise 1 : quelle PSK employer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChargeMessageUn {
    pub psk_id: String,
}

/// `server/hello` — première parole de Tune une fois le tuyau chiffré.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
}

/// `client/hello` — **la réponse qui fait la porte de sortie de S2-a**.
///
/// C'est ici que l'enceinte dit qui elle est et ce qu'elle sait faire. La
/// structure est délibérément permissive :
///
/// - tous les champs sont facultatifs sauf ce que nous savons nommer ;
/// - `reste` ramasse par `flatten` **tout** ce que nous n'avons pas déclaré.
///
/// La spécification bouge encore (le dépôt a été poussé le jour même de la
/// phase 1). Une structure stricte transformerait un champ nouveau en échec de
/// désérialisation, c'est-à-dire en connexion fermée sans explication — et
/// nous ferait perdre précisément la matière que cette brique existe pour
/// récolter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientHello {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Les rôles versionnés (`player@v1`, `metadata@v1`, …).
    #[serde(default)]
    pub supported_roles: Vec<String>,
    /// Les capacités du rôle `player` : codecs, fréquences, profondeurs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_support: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_info: Option<Value>,
    /// Les méthodes d'appairage proposées — matière pour S2-b, pas pour ici.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_pair_methods: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpaired_access: Option<Value>,
    /// Tout le reste, conservé tel quel.
    #[serde(flatten, default)]
    pub reste: Map<String, Value>,
}

impl ClientHello {
    /// Vrai si l'enceinte annonce une version quelconque du rôle `player`.
    ///
    /// Compare sur la famille (avant `@`) : `player@v1` et un futur
    /// `player@v2` doivent tous deux répondre oui, sans quoi la première
    /// version suivante nous rendrait aveugles.
    pub fn sait_jouer(&self) -> bool {
        self.supported_roles
            .iter()
            .any(|r| r.split('@').next() == Some("player"))
    }

    /// Les capacités du rôle `player`, en tenant compte de la clé **versionnée**.
    ///
    /// Mesuré sur le fil le 09/09/2026 contre l'implémentation de référence :
    /// le `client/hello` porte `player@v1_support`, **pas** `player_support`.
    /// La note de lecture de la phase 1 annonçait la forme non versionnée ;
    /// c'est le fil qui fait foi. L'implémentation de référence, côté serveur,
    /// lit d'ailleurs les deux et appelle la seconde « legacy ».
    ///
    /// On cherche donc, dans l'ordre :
    /// 1. `<rôle>_support` pour chaque rôle `player@vN` réellement annoncé —
    ///    ce qui suit automatiquement une future version `player@v2` ;
    /// 2. `player_support` non versionné, la forme héritée.
    ///
    /// Sans ça, Tune lirait `None` là où l'enceinte a décrit ses codecs, et
    /// S2-c choisirait un format à l'aveugle.
    pub fn support_du_lecteur(&self) -> Option<&Value> {
        for role in &self.supported_roles {
            if role.split('@').next() == Some("player") {
                if let Some(valeur) = self.reste.get(&format!("{role}_support")) {
                    return Some(valeur);
                }
            }
        }
        self.player_support
            .as_ref()
            .or_else(|| self.reste.get("player_support"))
    }

    /// Les familles de rôles annoncées, sans leur version.
    pub fn familles_de_roles(&self) -> Vec<&str> {
        self.supported_roles
            .iter()
            .filter_map(|r| r.split('@').next())
            .collect()
    }
}

/// `server/activate` — Tune déclare ce qu'il veut faire de cette connexion.
///
/// `activities` est ce qui départage plusieurs serveurs qui parlent à la même
/// enceinte (`playback` > `pairing` > vide). S2-a envoie une liste **vide** :
/// rien ne joue et rien ne s'appaire encore. C'est le message qui, envoyé sous
/// 30 s, empêche l'enceinte d'abandonner la connexion provisoire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerActivate {
    pub activities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_roles: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_client_hello_inconnu_n_est_jamais_perdu() {
        // Un champ que nous ne connaissons pas ne doit ni faire echouer la
        // lecture, ni disparaitre : c'est la matiere que S2-a existe pour
        // recolter, sur une specification qui bouge encore.
        let brut = r#"{
            "name": "Cuisine",
            "supported_roles": ["player@v1", "metadata@v1"],
            "player_support": {"codecs": ["opus", "flac", "pcm"]},
            "un_champ_du_futur": {"valeur": 42}
        }"#;
        let hello: ClientHello = serde_json::from_str(brut).expect("lecture permissive");
        assert_eq!(hello.name.as_deref(), Some("Cuisine"));
        assert!(
            hello.sait_jouer(),
            "player@v1 doit compter comme un lecteur"
        );
        assert_eq!(hello.familles_de_roles(), vec!["player", "metadata"]);
        assert!(
            hello.reste.contains_key("un_champ_du_futur"),
            "un champ inconnu doit etre CONSERVE, pas jete : {:?}",
            hello.reste
        );
    }

    #[test]
    fn les_capacites_du_lecteur_sont_lues_sous_leur_cle_versionnee() {
        // Trace reelle d'un `client/hello` recu le 09/09/2026 (aiosendspin git
        // HEAD) : la cle est `player@v1_support`. Lire `player_support` rendait
        // `None`, et c'est exactement ce que la premiere version de ce module
        // faisait.
        let brut = r#"{
            "name": "Preuve S2-a",
            "supported_roles": ["player@v1", "metadata@v1"],
            "player@v1_support": {
                "buffer_capacity": 2000000,
                "supported_commands": ["volume", "mute"],
                "supported_formats": [
                    {"bit_depth": 24, "channels": 2, "codec": "flac", "sample_rate": 48000}
                ]
            },
            "trust_level": "none"
        }"#;
        let hello: ClientHello = serde_json::from_str(brut).expect("lecture");
        assert!(
            hello.player_support.is_none(),
            "la cle NON versionnee est bien absente du fil reel"
        );
        let support = hello
            .support_du_lecteur()
            .expect("les capacites doivent etre trouvees sous player@v1_support");
        assert_eq!(support["buffer_capacity"], serde_json::json!(2_000_000));
        assert_eq!(
            support["supported_formats"][0]["codec"],
            serde_json::json!("flac")
        );
    }

    #[test]
    fn la_forme_non_versionnee_reste_lue_en_repli() {
        // L'implementation de reference la nomme « legacy » et la lit encore ;
        // nous aussi, sinon un lecteur d'avant la version des cles disparait.
        let hello = ClientHello {
            supported_roles: vec!["player@v1".into()],
            player_support: Some(serde_json::json!({"buffer_capacity": 42})),
            ..Default::default()
        };
        assert_eq!(
            hello.support_du_lecteur().expect("repli")["buffer_capacity"],
            serde_json::json!(42)
        );
    }

    #[test]
    fn une_version_de_role_a_venir_porte_aussi_ses_capacites() {
        // `player@v2_support` doit suivre `player@v2` sans qu'on touche au code.
        let brut = r#"{
            "supported_roles": ["player@v2"],
            "player@v2_support": {"buffer_capacity": 7}
        }"#;
        let hello: ClientHello = serde_json::from_str(brut).expect("lecture");
        assert_eq!(
            hello.support_du_lecteur().expect("v2")["buffer_capacity"],
            serde_json::json!(7)
        );
    }

    #[test]
    fn une_version_de_role_a_venir_compte_encore_comme_un_lecteur() {
        let hello = ClientHello {
            supported_roles: vec!["player@v2".into()],
            ..Default::default()
        };
        assert!(
            hello.sait_jouer(),
            "comparer la chaine entiere nous rendrait aveugles a player@v2"
        );
    }

    #[test]
    fn un_hello_sans_role_player_ne_pretend_pas_jouer() {
        let hello = ClientHello {
            supported_roles: vec!["metadata@v1".into(), "artwork@v1".into()],
            ..Default::default()
        };
        assert!(!hello.sait_jouer());
    }

    #[test]
    fn les_champs_absents_ne_sont_pas_serialises() {
        let activate = ServerActivate {
            activities: vec![],
            active_roles: None,
        };
        let texte = serde_json::to_string(&Enveloppe::nouvelle(TYPE_SERVER_ACTIVATE, activate))
            .expect("serialisation");
        assert!(
            !texte.contains("active_roles"),
            "un champ facultatif absent ne doit pas partir en null : {texte}"
        );
        assert!(texte.contains(r#""type":"server/activate""#));
        assert!(texte.contains(r#""activities":[]"#));
    }

    #[test]
    fn l_enveloppe_brute_lit_le_type_sans_toucher_a_la_charge() {
        let brute: EnveloppeBrute =
            serde_json::from_str(r#"{"type":"client/init","payload":{"n'importe":"quoi"}}"#)
                .expect("lecture");
        assert_eq!(brute.type_message, TYPE_CLIENT_INIT);
    }
}
