//! Côté MAÎTRE : appairer un agent, rattacher ses sorties à des zones, les
//! réinscrire au démarrage, l'oublier (#4626).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use super::sortie::{SortieAgentTune, device_id_maitre, nom_de_zone};
use super::{
    CLE_AGENTS, DemandeAppairage, ENTETE_JETON, PREFIXE_DEVICE_ID, ReponseAppairage, SortieExposee,
    TYPE_DE_SORTIE, identite, maintenant,
};
use crate::state::AppState;

const DELAI_APPAIRAGE: Duration = Duration::from_secs(10);
const DELAI_SONDE: Duration = Duration::from_secs(3);

/// Un agent appairé, tel que le maître le garde. Le jeton est en clair : il
/// faut le présenter. La clé est exclue des sauvegardes de configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAppaire {
    pub agent_id: String,
    pub nom: String,
    pub host: String,
    pub port: u16,
    pub jeton: String,
    pub appaire_le: i64,
}

impl AgentAppaire {
    pub fn base_url(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }

    /// Ce que l'API rend d'un agent : tout sauf le jeton.
    pub fn public(&self) -> Value {
        json!({
            "agent_id": self.agent_id,
            "nom": self.nom,
            "host": self.host,
            "port": self.port,
            "appaire_le": self.appaire_le,
        })
    }
}

/// Une zone du maître rattachée à une sortie d'agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZoneRattachee {
    pub zone_id: i64,
    pub device_id: String,
    pub nom: String,
}

pub fn agents(state: &AppState) -> Vec<AgentAppaire> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_AGENTS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn enregistrer_agents(state: &AppState, agents: &[AgentAppaire]) -> Result<(), String> {
    let json = serde_json::to_string(agents).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(state.backend.clone()).set(CLE_AGENTS, &json)
}

fn client() -> reqwest::Client {
    tune_core::http::client::builder()
        .build()
        .unwrap_or_default()
}

fn base_url(host: &str, port: u16) -> String {
    AgentAppaire {
        agent_id: String::new(),
        nom: String::new(),
        host: host.to_string(),
        port,
        jeton: String::new(),
        appaire_le: 0,
    }
    .base_url()
}

/// Appaire ce maître avec l'agent `host:port`, à l'aide du code affiché sur
/// l'agent. Les sorties de l'agent deviennent des zones du maître.
pub async fn appairer(
    state: &AppState,
    host: &str,
    port: u16,
    code: &str,
) -> Result<(AgentAppaire, Vec<ZoneRattachee>), String> {
    let (maitre_id, maitre_nom) = identite(state);
    let reponse = client()
        .post(format!("{}/agent-tune/appairer", base_url(host, port)))
        .timeout(DELAI_APPAIRAGE)
        .json(&DemandeAppairage {
            code: code.to_string(),
            maitre_id,
            maitre_nom,
        })
        .send()
        .await
        .map_err(|e| format!("serveur Tune injoignable : {e}"))?;
    if !reponse.status().is_success() {
        let motif = reponse.text().await.unwrap_or_default();
        return Err(if motif.is_empty() {
            "appairage refusé par l'agent".to_string()
        } else {
            motif
        });
    }
    let reponse: ReponseAppairage = reponse
        .json()
        .await
        .map_err(|e| format!("réponse d'appairage illisible : {e}"))?;
    let agent = AgentAppaire {
        agent_id: reponse.agent_id,
        nom: reponse.agent_nom,
        host: host.to_string(),
        port,
        jeton: reponse.jeton,
        appaire_le: maintenant(),
    };
    let mut liste = agents(state);
    liste.retain(|a| a.agent_id != agent.agent_id);
    liste.push(agent.clone());
    enregistrer_agents(state, &liste)?;
    tracing::info!(agent = %agent.nom, agent_id = %agent.agent_id, sorties = reponse.sorties.len(), "agent_tune_agent_appaire");
    let zones = inscrire_sorties(state, &agent, &reponse.sorties).await;
    Ok((agent, zones))
}

/// Inscrit chaque sortie de l'agent au registre du maître et lui rattache une
/// zone. L'appairage vaut consentement : comme pour un appareil ajouté à la
/// main (#3529), la zone naît même si la création automatique est décochée.
pub async fn inscrire_sorties(
    state: &AppState,
    agent: &AgentAppaire,
    sorties: &[SortieExposee],
) -> Vec<ZoneRattachee> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut zones = Vec::with_capacity(sorties.len());
    for sortie in sorties {
        let device_id = device_id_maitre(&agent.agent_id, &sortie.device_id);
        let nom = nom_de_zone(&sortie.nom, &agent.nom);
        {
            let mut registre = state.outputs.lock().await;
            registre.remove(&device_id);
            registre.register(Box::new(SortieAgentTune::new(agent, sortie)));
        }
        match repo.get_or_create(&nom, Some(TYPE_DE_SORTIE), &device_id) {
            Ok((zone_id, creee)) => {
                if !creee {
                    let _ = repo.set_online_by_device(&device_id, true);
                }
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::DeviceDiscovered,
                    json!({
                        "device_id": device_id,
                        "name": nom,
                        "device_type": TYPE_DE_SORTIE,
                        "host": agent.host,
                    }),
                );
                zones.push(ZoneRattachee {
                    zone_id,
                    device_id,
                    nom,
                });
            }
            Err(e) => {
                tracing::warn!(device_id = %device_id, error = %e, "agent_tune_zone_non_creee");
            }
        }
    }
    zones
}

/// Retire du registre les sorties d'un agent et met ses zones hors ligne.
/// Les zones sont GARDÉES (file, réglages) : un agent éteint revient.
async fn retirer_sorties(state: &AppState, agent_id: &str) {
    let prefixe = format!("{PREFIXE_DEVICE_ID}{agent_id}:");
    let retirees: Vec<String> = {
        let mut registre = state.outputs.lock().await;
        let ids: Vec<String> = registre
            .list()
            .into_iter()
            .filter(|id| id.starts_with(&prefixe))
            .collect();
        for id in &ids {
            registre.remove(id);
        }
        ids
    };
    let repo = ZoneRepo::with_backend(state.backend.clone());
    for id in retirees {
        let _ = repo.set_online_by_device(&id, false);
    }
}

/// Lit les sorties d'un agent appairé (`None` s'il est injoignable ou s'il a
/// révoqué l'appairage).
pub async fn lire_sorties(agent: &AgentAppaire) -> Result<Vec<SortieExposee>, String> {
    let reponse = client()
        .get(format!("{}/agent-tune/sorties", agent.base_url()))
        .header(ENTETE_JETON, &agent.jeton)
        .timeout(DELAI_SONDE)
        .send()
        .await
        .map_err(|e| format!("agent injoignable : {e}"))?;
    if !reponse.status().is_success() {
        return Err(format!("agent : {}", reponse.status()));
    }
    reponse
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))
}

/// Au démarrage : réinscrit les sorties de chaque agent joignable.
pub async fn reinscrire_les_agents(state: &AppState) {
    for agent in agents(state) {
        match lire_sorties(&agent).await {
            Ok(sorties) => {
                inscrire_sorties(state, &agent, &sorties).await;
            }
            Err(e) => {
                tracing::info!(agent = %agent.nom, error = %e, "agent_tune_agent_hors_ligne");
                retirer_sorties(state, &agent.agent_id).await;
            }
        }
    }
}

/// Oublie un agent : ses sorties quittent le registre, ses zones passent hors
/// ligne, et l'agent est prié (au mieux) d'oublier ce maître.
pub async fn oublier(state: &AppState, agent_id: &str) -> bool {
    let mut liste = agents(state);
    let Some(position) = liste.iter().position(|a| a.agent_id == agent_id) else {
        return false;
    };
    let agent = liste.remove(position);
    if let Err(e) = enregistrer_agents(state, &liste) {
        tracing::warn!(error = %e, "agent_tune_agent_non_retire");
        return false;
    }
    retirer_sorties(state, agent_id).await;
    let _ = client()
        .post(format!("{}/agent-tune/oublier", agent.base_url()))
        .header(ENTETE_JETON, &agent.jeton)
        .timeout(DELAI_SONDE)
        .send()
        .await;
    tracing::info!(agent = %agent.nom, agent_id = %agent_id, "agent_tune_agent_oublie");
    true
}

/// L'annonce publique d'un serveur Tune (`GET /agent-tune/annonce`).
async fn sonder_annonce(host: &str, port: u16) -> Option<Value> {
    let reponse = client()
        .get(format!("{}/agent-tune/annonce", base_url(host, port)))
        .timeout(DELAI_SONDE)
        .send()
        .await
        .ok()?;
    if !reponse.status().is_success() {
        return None;
    }
    reponse.json().await.ok()
}

/// Les serveurs Tune du réseau qui savent être agents : la découverte mDNS
/// unie au registre manuel (`/system/peers`), sondés un à un.
pub async fn candidats(state: &AppState, decouverts: &[Value]) -> Vec<Value> {
    let (moi, _) = identite(state);
    let appaires: Vec<String> = agents(state).into_iter().map(|a| a.agent_id).collect();
    let pairs = crate::routes::system::peers_payload(state, decouverts).await;
    let mut rendu = Vec::new();
    for pair in pairs.as_array().cloned().unwrap_or_default() {
        let (Some(host), Some(port)) = (
            pair.get("host").and_then(Value::as_str),
            pair.get("port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok()),
        ) else {
            continue;
        };
        let Some(annonce) = sonder_annonce(host, port).await else {
            continue;
        };
        let agent_id = annonce
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if agent_id.is_empty() || agent_id == moi {
            continue;
        }
        rendu.push(json!({
            "agent_id": agent_id,
            "nom": annonce.get("nom").cloned().unwrap_or(Value::Null),
            "version": annonce.get("version").cloned().unwrap_or(Value::Null),
            "host": host,
            "port": port,
            "appaire": appaires.contains(&agent_id),
        }));
    }
    rendu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_adresse_ipv6_est_mise_entre_crochets() {
        assert_eq!(base_url("192.0.2.1", 8888), "http://192.0.2.1:8888");
        assert_eq!(base_url("fe80::1", 8888), "http://[fe80::1]:8888");
    }
}
