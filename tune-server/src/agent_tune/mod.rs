//! Rôle maître / agent entre deux serveurs Tune du réseau local (#4626).
//!
//! Un serveur Tune **agent** (un Raspberry Pi branché à un DAC, un PC sous
//! Windows…) prête ses sorties LOCALES à un serveur Tune **maître**. Le maître
//! les voit comme des zones ordinaires et y joue sa bibliothèque ; l'agent
//! joue le flux sur sa sortie, sans repasser par sa propre chaîne.
//!
//! ## Le transport retenu, et pourquoi ce n'est pas OAAT
//!
//! OAAT pousse des paquets PCM horodatés vers un *endpoint* qui les écrit dans
//! un périphérique (`oaat-endpoint`, HAL `write_frames`). Les sorties locales
//! de Tune, elles, TIRENT un flux HTTP (`OutputTarget::play_media` reçoit une
//! URL) : WASAPI exclusif, ASIO, CoreAudio exclusif, ALSA `hw:` — tout le
//! savoir-faire bit-perfect de Tune vit derrière cette porte-là. Brancher un
//! endpoint OAAT sur l'agent obligerait soit à écrire dans le périphérique
//! par cpal (on perd le mode exclusif, et la sortie se dispute le DAC avec la
//! zone locale de l'agent), soit à republier le PCM reçu en flux HTTP local
//! pour la sortie locale (deux tampons, une latence de plus, pour rien).
//!
//! Cette première version relaie donc les ORDRES en HTTP (`/agent-tune/…`,
//! authentifiés par un jeton d'appairage) et laisse la sortie locale de
//! l'agent TIRER le flux du maître (`/stream/…`), exactement comme le fait un
//! renderer réseau. OAAT reste le bon candidat pour la suite : synchroniser
//! une zone d'agent avec d'autres endpoints OAAT d'un même groupe.
//!
//! ## Le chemin du signal
//!
//! ```text
//! maître : décodage → DSP de la zone du maître (EQ, convolution…, s'il y en a)
//!          → /stream/… (octets d'origine quand aucun traitement n'est actif)
//!   ── HTTP, réseau local ──▶
//! agent  : sortie locale (décodage, mode exclusif, volume de la sortie) → DAC
//! ```
//!
//! La chaîne de l'agent (EQ, convolveur, trim de SA zone) n'est JAMAIS
//! traversée : la commande arrive directement à la sortie, pas à
//! l'orchestrateur de l'agent. Une seule chaîne, celle du maître. Le type de
//! sortie `tune_agent` est un type « pull » pour l'orchestrateur du maître
//! (`is_pull_dsp_output_type`) : sans DSP actif, il reçoit les octets tels
//! quels — c'est la condition du bit-perfect.
//!
//! ## L'appairage
//!
//! Aucun accès libre sur le réseau : l'agent ne sert ses sorties qu'à un
//! maître qui présente un jeton, et ce jeton ne s'obtient qu'avec un code à
//! six chiffres affiché SUR l'agent (`POST /api/v1/agent-tune/agent/code`),
//! valable cinq minutes, à usage unique, cinq essais.

pub mod agent;
pub mod maitre;
pub mod sortie;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::traits::OutputCapabilities;

use crate::state::AppState;

/// `output_type` des sorties qu'un maître tient d'un agent.
pub const TYPE_DE_SORTIE: &str = "tune_agent";
/// Préfixe du `device_id` côté maître : `tune-agent:{agent_id}:{device_id de l'agent}`.
pub const PREFIXE_DEVICE_ID: &str = "tune-agent:";
/// L'en-tête qui porte le jeton d'appairage, du maître vers l'agent.
pub const ENTETE_JETON: &str = "x-tune-agent-jeton";
/// Identité stable de CE serveur dans le rôle maître/agent.
pub const CLE_IDENTITE: &str = "agent_tune_identite";
/// Côté agent : les maîtres appairés (empreintes de jeton, jamais le jeton).
pub const CLE_MAITRES: &str = "agent_tune_maitres";
/// Côté maître : les agents appairés, avec leur jeton.
pub const CLE_AGENTS: &str = "agent_tune_agents";
/// Les types de sortie qu'un agent prête. Les renderers réseau en sont exclus
/// (le maître les voit lui-même), et `tune_agent` aussi : un maître qui est
/// lui-même agent ne re-prête pas les sorties qu'on lui a prêtées.
pub const TYPES_EXPOSES: &[&str] = &["local"];

/// Ce que l'agent tient en mémoire : le code d'appairage en cours et les baux.
#[derive(Default)]
pub struct EtatAgentTune {
    pub(crate) code: std::sync::Mutex<Option<agent::CodeEnCours>>,
    /// device_id LOCAL → le maître qui tient la sortie, et le flux qu'il y joue.
    pub(crate) baux: std::sync::Mutex<HashMap<String, Bail>>,
}

/// Une sortie de l'agent tenue par un maître.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bail {
    pub maitre_id: String,
    pub url: String,
}

/// Une sortie que l'agent prête, telle que le maître la reçoit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortieExposee {
    pub device_id: String,
    pub nom: String,
    pub capacites: OutputCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemandeAppairage {
    pub code: String,
    pub maitre_id: String,
    pub maitre_nom: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReponseAppairage {
    pub agent_id: String,
    pub agent_nom: String,
    pub jeton: String,
    pub sorties: Vec<SortieExposee>,
}

/// Un ordre du maître pour UNE sortie de l'agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "commande", rename_all = "snake_case")]
pub enum Commande {
    Lire {
        url: String,
        mime_type: String,
        #[serde(default)]
        titre: Option<String>,
        #[serde(default)]
        artiste: Option<String>,
        #[serde(default)]
        album: Option<String>,
        #[serde(default)]
        pochette: Option<String>,
        #[serde(default)]
        duree_ms: Option<u64>,
        #[serde(default)]
        direct: bool,
    },
    Pause,
    Reprendre,
    Arreter,
    Position {
        position_ms: u64,
    },
    Volume {
        volume: f64,
    },
    Muet {
        muet: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrdreSortie {
    pub device_id: String,
    pub commande: Commande,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemandeEtat {
    pub device_id: String,
}

/// L'état d'une sortie de l'agent, vu par le maître qui la tient.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EtatSortieDistante {
    /// `true` si CE maître tient la sortie. `false` : la sortie est libre ou
    /// a été reprise sur place — l'état est alors rendu « arrêté ».
    pub tenue: bool,
    pub statut: tune_core::outputs::traits::OutputStatus,
}

/// L'identité de ce serveur : un identifiant stable, créé au premier besoin,
/// et le nom que l'utilisateur lui a donné (#2110).
pub fn identite(state: &AppState) -> (String, String) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let id = match settings
        .get(CLE_IDENTITE)
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
    {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().simple().to_string();
            if let Err(e) = settings.set(CLE_IDENTITE, &id) {
                tracing::warn!(error = %e, "agent_tune_identite_non_persistee");
            }
            id
        }
    };
    let nom = crate::routes::system::resolve_server_name(
        settings.get("server_name").ok().flatten().as_deref(),
    );
    (id, format!("Tune ({nom})"))
}

/// Empreinte SHA-256 (hexadécimal) d'un secret : seul ce que l'agent garde.
pub fn empreinte(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

/// Un secret de 244 bits aléatoires (deux UUID v4).
pub(crate) fn secret_aleatoire() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub(crate) fn maintenant() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_commande_lire_a_une_forme_stable_sur_le_fil() {
        let c = Commande::Lire {
            url: "http://192.0.2.1:8888/stream/1.flac".into(),
            mime_type: "audio/flac".into(),
            titre: Some("T".into()),
            artiste: None,
            album: None,
            pochette: None,
            duree_ms: Some(1000),
            direct: false,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["commande"], "lire");
        assert_eq!(v["url"], "http://192.0.2.1:8888/stream/1.flac");
        let relu: Commande = serde_json::from_value(v).unwrap();
        assert_eq!(relu, c);
        let pause: Commande = serde_json::from_str(r#"{"commande":"pause"}"#).unwrap();
        assert_eq!(pause, Commande::Pause);
    }

    #[test]
    fn l_empreinte_ne_rend_pas_le_secret() {
        let s = secret_aleatoire();
        assert_eq!(s.len(), 64);
        let e = empreinte(&s);
        assert_eq!(e.len(), 64);
        assert_ne!(e, s);
        assert_eq!(e, empreinte(&s));
    }
}
