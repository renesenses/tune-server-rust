//! Sendspin — couche de description des appareils (phase 1 du chantier #3326).
//!
//! Sendspin est le protocole multi-pièces de l'Open Home Foundation. Ce module
//! ne fait qu'UNE chose : lire une annonce mDNS Sendspin et la décrire. Il ne
//! joue rien, n'ouvre aucune socket, n'enregistre aucune sortie. La lecture est
//! la phase 2 ; un protocole audio à moitié branché serait pire que rien.
//!
//! # Le vocabulaire, qui est le premier piège
//!
//! Dans Sendspin, **le « client » est l'ENCEINTE** et le « serveur » est la
//! source de musique :
//!
//! > « The Sendspin client is always the consumer of data like audio or
//! > metadata, regardless of who initiated the connection. »
//! > — <https://github.com/Sendspin/spec/blob/main/connection.md>
//!
//! Pour envoyer de la musique vers une enceinte Sendspin, Tune doit donc parler
//! le **rôle serveur** du protocole, même si, du point de vue de Tune, il
//! s'agit d'ajouter une « sortie ». C'est l'inverse de la lecture naïve du
//! ticket #3326, et cela change l'ampleur de la phase 2 (cf. la PR).
//!
//! # Découverte, telle que la spécification la décrit
//!
//! Deux modes, relevés dans `connection.md` (§ Establishing a Connection) le
//! 08/09/2026 :
//!
//! - **Serveur-initié (recommandé)** : c'est le LECTEUR qui s'annonce, en
//!   `_sendspin._tcp.local.`, port recommandé `8928`, avec un enregistrement
//!   TXT `path` **REQUIS** (valeur recommandée `/sendspin`) et un TXT `name`
//!   facultatif. Le serveur découvre les lecteurs et compose vers chacun en
//!   WebSocket.
//! - **Client-initié** : c'est le SERVEUR qui s'annonce, en
//!   `_sendspin-server._tcp.local.`, port recommandé `8927`, mêmes TXT.
//!
//! > « Servers must support both methods described below. »
//!
//! Tune n'implémente ici que la moitié « découvrir les lecteurs » du mode
//! recommandé : c'est celle qui remonte des appareils à l'utilisateur sans rien
//! promettre. L'annonce `_sendspin-server._tcp` n'est PAS faite — s'annoncer
//! comme serveur Sendspin ferait composer les enceintes vers un serveur qui
//! raccrocherait aussitôt, ce qui est exactement le « à moitié branché » que
//! cette phase refuse. La constante existe pour la phase 2.
//!
//! # Ce que l'annonce ne contient PAS
//!
//! La spécification ne définit **que** `path` et `name`. Il n'y a **aucun**
//! identifiant dans le TXT. L'identité durable d'un lecteur Sendspin est son
//! `client_id`, c'est-à-dire sa clé publique Curve25519 encodée en base64url
//! sans remplissage (43 caractères) — et elle n'est connue qu'APRÈS la poignée
//! de main Noise (`connection.md` § Identities). Conséquence directe pour
//! nous : à la découverte, `stable_id` reste `None` et l'identifiant d'appareil
//! retombe sur la forme dérivée de l'adresse (`legacy_device_id`), avec tout ce
//! que #1528 reproche à cette forme. C'est un manque de nos abstractions, pas
//! un oubli : voir la PR du chantier.

use std::collections::HashMap;

use serde_json::Value;

use super::device::{DiscoveredDevice, OutputType};

/// Service mDNS annoncé par un **lecteur** Sendspin (mode serveur-initié).
pub const SERVICE_LECTEUR: &str = "_sendspin._tcp.local.";

/// Service mDNS que devrait annoncer un **serveur** Sendspin (mode
/// client-initié). Déclaré ici pour la phase 2 ; Tune ne l'annonce pas encore.
pub const SERVICE_SERVEUR: &str = "_sendspin-server._tcp.local.";

/// Port recommandé par la spécification pour un lecteur.
///
/// Recommandé, pas imposé : le port réel est celui de l'annonce. Cette valeur
/// ne sert que de repli quand l'annonce n'en porte pas.
pub const PORT_LECTEUR_RECOMMANDE: u16 = 8928;

/// Port recommandé par la spécification pour un serveur (phase 2).
pub const PORT_SERVEUR_RECOMMANDE: u16 = 8927;

/// Valeur recommandée du TXT `path`. **Jamais utilisée comme défaut** : le TXT
/// est REQUIS, et inventer un chemin ferait composer vers une URL qui n'existe
/// pas.
pub const CHEMIN_RECOMMANDE: &str = "/sendspin";

/// Clé de `DiscoveredDevice::capabilities` où le chemin WebSocket est rangé.
pub const CLE_CHEMIN: &str = "sendspin_path";

/// Clé de `DiscoveredDevice::capabilities` marquant un appareil Sendspin.
pub const CLE_SENDSPIN: &str = "sendspin";

/// Motif rendu pour tout appareil Sendspin : la lecture n'existe pas encore.
pub const MOTIF_PHASE_DECOUVERTE: &str = "sendspin_lecture_non_implementee_phase_1";

/// Motif rendu quand l'annonce ne porte pas le TXT `path`, pourtant REQUIS :
/// l'appareil est vu, mais aucune URL WebSocket ne peut être construite.
pub const MOTIF_CHEMIN_ABSENT: &str = "sendspin_txt_path_absent";

/// Ce qu'une annonce mDNS Sendspin porte vraiment : deux champs, pas un de plus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annonce {
    /// TXT `path` — REQUIS par la spécification. `None` quand l'appareil ne
    /// l'annonce pas : il est alors hors norme et injoignable.
    pub chemin: Option<String>,
    /// TXT `name` — facultatif, et seulement une « discovery-time hint » :
    /// « if the two differ, the `client/hello` value is authoritative ».
    pub nom: Option<String>,
}

impl Annonce {
    /// Lit les deux TXT. Une valeur vide ou blanche vaut absente : un TXT
    /// présent mais vide n'est pas un chemin.
    pub fn depuis_txt(chemin: Option<&str>, nom: Option<&str>) -> Self {
        Self {
            chemin: normalise(chemin),
            nom: normalise(nom),
        }
    }

    /// Les capacités à ranger sur le `DiscoveredDevice`.
    pub fn capacites(&self) -> HashMap<String, Value> {
        let mut caps = HashMap::new();
        caps.insert(CLE_SENDSPIN.to_string(), Value::Bool(true));
        if let Some(chemin) = &self.chemin {
            caps.insert(CLE_CHEMIN.to_string(), Value::String(chemin.clone()));
        }
        caps
    }
}

fn normalise(valeur: Option<&str>) -> Option<String> {
    valeur
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// L'URL WebSocket vers laquelle un serveur Sendspin composerait.
///
/// `ws://` et non `wss://` : « The WebSocket transport MUST be plain `ws://`.
/// Confidentiality and integrity are provided end to end by the Noise layer
/// inside the WebSocket payloads. » (`connection.md`). Le chiffrement n'est pas
/// absent, il est ailleurs.
///
/// Une adresse IPv6 est mise entre crochets : `pick_best_address` peut rendre
/// une IPv6 nue quand l'appareil n'annonce aucune IPv4, et `ws://fe80::1:8928`
/// n'est pas une URL.
pub fn url_websocket(hote: &str, port: u16, chemin: &str) -> String {
    let hote = if hote.contains(':') && !hote.starts_with('[') {
        format!("[{hote}]")
    } else {
        hote.to_string()
    };
    if chemin.starts_with('/') {
        format!("ws://{hote}:{port}{chemin}")
    } else {
        format!("ws://{hote}:{port}/{chemin}")
    }
}

/// La description d'UN appareil Sendspin découvert, telle qu'un client HTTP la
/// reçoit.
///
/// `playable` vaut **toujours** `false` en phase 1, et le motif dit pourquoi.
/// Ce n'est pas décoratif : c'est ce qui empêche une interface d'offrir une
/// zone qui ne jouerait rien.
pub fn decrire_un(appareil: &DiscoveredDevice) -> Value {
    let chemin = appareil
        .capabilities
        .get(CLE_CHEMIN)
        .and_then(Value::as_str)
        .map(str::to_string);

    let (url, motif) = match &chemin {
        Some(chemin) => (
            Value::String(url_websocket(&appareil.host, appareil.port, chemin)),
            MOTIF_PHASE_DECOUVERTE,
        ),
        None => (Value::Null, MOTIF_CHEMIN_ABSENT),
    };

    serde_json::json!({
        "id": appareil.id,
        "name": appareil.name,
        "type": OutputType::Sendspin.to_string(),
        "host": appareil.host,
        "port": appareil.port,
        "path": chemin,
        "websocket_url": url,
        "playable": false,
        "reason": motif,
    })
}

/// Décrit tous les appareils Sendspin d'une liste de découverte, en ignorant
/// les autres protocoles.
pub fn decrire(appareils: &[DiscoveredDevice]) -> Vec<Value> {
    appareils
        .iter()
        .filter(|a| a.device_type == OutputType::Sendspin)
        .map(decrire_un)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appareil(host: &str, port: u16, chemin: Option<&str>) -> DiscoveredDevice {
        let mut d = DiscoveredDevice::new(
            format!("sendspin-{host}-{port}"),
            "Cuisine".into(),
            OutputType::Sendspin,
            host.into(),
            port,
        );
        d.capabilities = Annonce::depuis_txt(chemin, None).capacites();
        d
    }

    #[test]
    fn annonce_lit_les_deux_txt_et_ecarte_le_vide() {
        let a = Annonce::depuis_txt(Some("/sendspin"), Some("  Cuisine  "));
        assert_eq!(a.chemin.as_deref(), Some("/sendspin"));
        assert_eq!(a.nom.as_deref(), Some("Cuisine"));

        // Un TXT présent mais vide n'est pas une valeur.
        let vide = Annonce::depuis_txt(Some("   "), Some(""));
        assert_eq!(vide, Annonce::default());
    }

    #[test]
    fn le_chemin_absent_n_est_jamais_remplace_par_le_chemin_recommande() {
        // Le TXT `path` est REQUIS. Un appareil qui ne l'annonce pas est hors
        // norme : lui inventer `/sendspin` fabriquerait une URL qui n'existe
        // pas, et la phase 2 composerait dans le vide.
        let a = Annonce::depuis_txt(None, Some("Cuisine"));
        assert!(a.chemin.is_none());
        assert!(!a.capacites().contains_key(CLE_CHEMIN));

        let decrit = decrire_un(&appareil("192.168.1.42", 8928, None));
        assert_eq!(decrit["websocket_url"], Value::Null);
        assert_eq!(decrit["reason"], MOTIF_CHEMIN_ABSENT);
        assert_eq!(decrit["path"], Value::Null);
    }

    #[test]
    fn url_websocket_est_en_clair_et_encadre_l_ipv6() {
        assert_eq!(
            url_websocket("192.168.1.42", 8928, "/sendspin"),
            "ws://192.168.1.42:8928/sendspin"
        );
        // Un chemin sans barre oblique de tête reste un chemin.
        assert_eq!(
            url_websocket("192.168.1.42", 8928, "sendspin"),
            "ws://192.168.1.42:8928/sendspin"
        );
        assert_eq!(
            url_websocket("fe80::1", 8928, "/sendspin"),
            "ws://[fe80::1]:8928/sendspin"
        );
        // Déjà encadrée : on ne double pas les crochets.
        assert_eq!(
            url_websocket("[fe80::1]", 8928, "/sendspin"),
            "ws://[fe80::1]:8928/sendspin"
        );
    }

    #[test]
    fn un_appareil_sendspin_n_est_jamais_annonce_jouable() {
        let decrit = decrire_un(&appareil("192.168.1.42", 8928, Some("/sendspin")));
        assert_eq!(decrit["playable"], Value::Bool(false));
        assert_eq!(decrit["reason"], MOTIF_PHASE_DECOUVERTE);
        assert_eq!(decrit["websocket_url"], "ws://192.168.1.42:8928/sendspin");
        assert_eq!(decrit["type"], "sendspin");
    }

    #[test]
    fn decrire_ignore_les_autres_protocoles() {
        let autre = DiscoveredDevice::new(
            "dlna-1".into(),
            "Marantz".into(),
            OutputType::Dlna,
            "192.168.1.50".into(),
            80,
        );
        let liste = vec![appareil("192.168.1.42", 8928, Some("/sendspin")), autre];
        let decrit = decrire(&liste);
        assert_eq!(decrit.len(), 1);
        assert_eq!(decrit[0]["host"], "192.168.1.42");
    }

    #[test]
    fn le_port_recommande_reste_un_repli_pas_une_verite() {
        // Un lecteur qui annonce 9000 est joint sur 9000, pas sur 8928.
        let decrit = decrire_un(&appareil("192.168.1.42", 9000, Some("/sendspin")));
        assert_eq!(decrit["port"], 9000);
        assert_ne!(decrit["port"], PORT_LECTEUR_RECOMMANDE);
        assert_eq!(decrit["websocket_url"], "ws://192.168.1.42:9000/sendspin");
    }
}
