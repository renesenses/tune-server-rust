//! Le **mode de transition** : accepter, ou non, un `client/hello` en clair.
//!
//! ## Pourquoi ce mode existe
//!
//! Mesuré le 11/09/2026 sur la version PUBLIÉE d'`aiosendspin` (6.0.5, celle
//! dont dépend le lecteur de référence `sendspin` 7.5.0) :
//!
//! - le paquet n'embarque **aucun module `noise/`** ;
//! - il ne connaît **ni `client/init` ni `server/activate`** — aucune de ces
//!   deux chaînes n'apparaît dans son code.
//!
//! Autrement dit : le Sendspin chiffré est spécifié et implémenté dans le dépôt
//! git de la fondation, mais **aucun lecteur publié ne le parle**. Un serveur
//! qui n'implémente que la branche chiffrée — ce que S2-a livrait — est
//! conforme et ne peut parler à aucune enceinte installée.
//!
//! L'implémentation de référence règle ce cas exactement ainsi : un drapeau
//! `allow_unencrypted`, **faux par défaut**, passé au constructeur du serveur.
//!
//! ## Trois choses que ce module tient
//!
//! 1. **C'est un CHOIX, jamais un repli.** Il n'existe aucun chemin où Tune
//!    bascule en clair parce que le chiffré a échoué : l'aiguillage se fait sur
//!    le TYPE du premier message reçu, et le clair n'est admis que si ce mode
//!    est explicitement armé. Un client qui propose Noise obtient Noise, mode
//!    de transition armé ou non.
//! 2. **Le défaut est le refus.** Justifié, et pas seulement par prudence : à
//!    ce stade du chantier rien n'est offert derrière la poignée de main (ni
//!    son, ni commande, ni donnée de bibliothèque). Armer le clair par défaut
//!    ouvrirait un point d'accès non chiffré sur le réseau local **sans aucun
//!    gain fonctionnel**. C'est aussi le défaut de l'implémentation de
//!    référence.
//! 3. **L'état est lisible.** [`ModeTransition::decrire`] le publie dans
//!    `GET /devices/sendspin`, et chaque session enregistrée dit par quel
//!    transport elle est passée.
//!
//! ## Ce que le mode de transition ne répare pas, et aggrave
//!
//! La PSK employée est la **Sentinelle**, une constante publiée : même chiffré,
//! **le pair n'est pas authentifié** (c'est le travail de S2-b, l'appairage
//! CPace). En clair, il n'y a même plus de canal chiffré : le `client_id` est
//! une simple *prétention*, que n'importe qui sur le réseau local peut écrire.
//! C'est précisément pourquoi rien ne doit être branché derrière tant que S2-b
//! n'est pas livrée.

use serde_json::{Value, json};

/// Le nom du réglage, écrit une seule fois.
///
/// C'est une variable d'environnement, comme `TUNE_NOTIFICATIONS_ENABLED` : le
/// dépôt a déjà ce vocabulaire pour les interrupteurs de fonctionnalité, et un
/// réglage de sécurité doit pouvoir être lu par un administrateur dans l'unité
/// systemd sans ouvrir de base de données.
pub const VARIABLE_ENVIRONNEMENT: &str = "TUNE_SENDSPIN_ALLOW_UNENCRYPTED";

/// Ce que Tune admet comme premier message sur le point d'accès Sendspin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeTransition {
    /// Seule la poignée de main Noise est admise. **C'est le défaut.**
    #[default]
    ChiffrementSeul,
    /// Un `client/hello` en clair est admis en plus, jamais à la place.
    ClairAccepte,
}

impl ModeTransition {
    /// Vrai si un `client/hello` en clair est admis.
    #[must_use]
    pub fn accepte_le_clair(self) -> bool {
        matches!(self, Self::ClairAccepte)
    }

    /// Le nom du mode, pour le journal et le diagnostic.
    #[must_use]
    pub fn nom(self) -> &'static str {
        match self {
            Self::ChiffrementSeul => "chiffrement_seul",
            Self::ClairAccepte => "clair_accepte",
        }
    }

    /// Lit le mode d'une valeur de réglage, **sans toucher à l'environnement**.
    ///
    /// Fonction pure, donc éprouvable : un témoin qui devrait poser une
    /// variable d'environnement pour mesurer ce choix casserait la suite
    /// `--workspace` (`std::env::set_var` est global au processus et les
    /// binaires de test tournent sur plusieurs fils).
    ///
    /// Tout ce qui n'est pas une affirmation explicite vaut refus : une valeur
    /// mal orthographiée ne doit jamais ouvrir la porte.
    #[must_use]
    pub fn depuis_valeur(valeur: Option<&str>) -> Self {
        match valeur.map(|v| v.trim().to_lowercase()) {
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Self::ClairAccepte,
            _ => Self::ChiffrementSeul,
        }
    }

    /// Le mode en vigueur pour ce processus, lu **une seule fois**.
    ///
    /// Une seule lecture, et donc une seule vérité : le routeur qui monte le
    /// point d'accès et la route de diagnostic qui l'expose ne peuvent pas se
    /// contredire. Sans ce cache, un réglage changé en cours de route ferait
    /// dire au diagnostic l'inverse de ce que le point d'accès applique.
    #[must_use]
    pub fn en_vigueur() -> Self {
        static MODE: std::sync::OnceLock<ModeTransition> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            Self::depuis_valeur(std::env::var(VARIABLE_ENVIRONNEMENT).ok().as_deref())
        })
    }

    /// La description JSON du mode, pour `GET /devices/sendspin`.
    ///
    /// Un testeur doit pouvoir répondre à « ce serveur accepte-t-il du clair ? »
    /// sans lire le code ni le journal.
    #[must_use]
    pub fn decrire(self) -> Value {
        json!({
            "setting": VARIABLE_ENVIRONNEMENT,
            "mode": self.nom(),
            "unencrypted_accepted": self.accepte_le_clair(),
            // Écrit noir sur blanc : même chiffré, la PSK Sentinelle est
            // publique et n'authentifie personne (S2-b).
            "peer_authenticated": false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_defaut_refuse_le_clair() {
        // La porte fermée est le defaut, des trois cotes : le type par defaut,
        // l'absence de reglage, et un reglage vide.
        assert_eq!(ModeTransition::default(), ModeTransition::ChiffrementSeul);
        assert!(!ModeTransition::default().accepte_le_clair());
        assert!(!ModeTransition::depuis_valeur(None).accepte_le_clair());
        assert!(!ModeTransition::depuis_valeur(Some("")).accepte_le_clair());
    }

    #[test]
    fn seule_une_affirmation_explicite_ouvre_la_porte() {
        for oui in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(
                ModeTransition::depuis_valeur(Some(oui)).accepte_le_clair(),
                "{oui:?} doit armer le mode de transition"
            );
        }
        // Tout le reste refuse. « maybe » et « faux » comptent : une valeur mal
        // orthographiee ne doit pas ouvrir un point d'acces non chiffre.
        for non in ["0", "false", "no", "off", "oui", "maybe", "enabled", "tru"] {
            assert!(
                !ModeTransition::depuis_valeur(Some(non)).accepte_le_clair(),
                "{non:?} ne doit PAS armer le mode de transition"
            );
        }
    }

    #[test]
    fn la_description_nomme_le_reglage_et_son_etat() {
        let ferme = ModeTransition::ChiffrementSeul.decrire();
        assert_eq!(ferme["setting"], json!(VARIABLE_ENVIRONNEMENT));
        assert_eq!(ferme["unencrypted_accepted"], json!(false));
        assert_eq!(ferme["mode"], json!("chiffrement_seul"));

        let ouvert = ModeTransition::ClairAccepte.decrire();
        assert_eq!(ouvert["unencrypted_accepted"], json!(true));
        assert_eq!(ouvert["mode"], json!("clair_accepte"));
        assert_eq!(
            ouvert["peer_authenticated"],
            json!(false),
            "meme chiffre, la PSK Sentinelle est publique : rien n'authentifie le pair"
        );
    }
}
