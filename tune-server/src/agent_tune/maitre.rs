//! Côté MAÎTRE : appairer un agent, rattacher ses sorties à des zones, les
//! réinscrire au démarrage, l'oublier (#4626).
//!
//! Le sort des zones d'un agent (décision de Bertrand, 08/10) :
//! - l'agent ne répond plus : ses zones passent HORS LIGNE et sont gardées
//!   (file, réglages) — un agent éteint revient ;
//! - l'appairage est RÉVOQUÉ, d'un côté ou de l'autre : ses zones sont
//!   SUPPRIMÉES chez le maître, comme par `DELETE /zones/{id}` (arrêtées si
//!   elles jouent, puis masquées). Un nouvel appairage vaut consentement et
//!   les fait réapparaître.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::PlayState;

use super::sortie::{SortieAgentTune, device_id_maitre, nom_de_zone};
use super::{
    CLE_AGENTS, DemandeAppairage, ENTETE_JETON, PREFIXE_DEVICE_ID, ReponseAppairage, SortieExposee,
    TYPE_DE_SORTIE, identite, maintenant,
};
use crate::state::AppState;

const DELAI_APPAIRAGE: Duration = Duration::from_secs(10);
const DELAI_SONDE: Duration = Duration::from_secs(3);
/// Le temps laissé à l'arrêt d'une zone avant sa suppression, comme pour
/// `DELETE /zones/{id}` (#5322) : un agent muet ne fait pas attendre.
const DELAI_ARRET: Duration = Duration::from_secs(10);

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
            maitre_port: Some(state.port),
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
    // L'appairage vaut consentement : une zone supprimée par une révocation
    // précédente (ou à la main) revient, puisque l'utilisateur vient de
    // redemander ces sorties.
    let repo = ZoneRepo::with_backend(state.backend.clone());
    for zone in &zones {
        if repo.is_device_hidden(&zone.device_id)
            && let Err(e) = repo.unhide(zone.zone_id)
        {
            tracing::warn!(zone_id = zone.zone_id, error = %e, "agent_tune_zone_non_demasquee");
        }
    }
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
/// Les zones sont GARDÉES (file, réglages) : un agent éteint revient. La
/// révocation, elle, passe aussi par [`supprimer_zones`].
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

/// Supprime chez le maître les zones d'un agent dont l'appairage est révoqué,
/// comme le fait `DELETE /zones/{id}` : une zone qui joue est d'abord arrêtée,
/// puis masquée, et `ZoneDeleted` est émis.
async fn supprimer_zones(state: &AppState, agent_id: &str) {
    let prefixe = format!("{PREFIXE_DEVICE_ID}{agent_id}:");
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = match repo.list() {
        Ok(zones) => zones,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "agent_tune_zones_illisibles");
            return;
        }
    };
    for zone in zones {
        let Some(id) = zone.id else { continue };
        let Some(device_id) = zone
            .output_device_id
            .as_deref()
            .filter(|d| d.starts_with(&prefixe))
        else {
            continue;
        };
        if state.playback.get_state(id).await.state != PlayState::Stopped {
            let arret = state.orchestrator.stop(id, Some(device_id));
            if tokio::time::timeout(DELAI_ARRET, arret).await.is_err() {
                tracing::warn!(
                    zone_id = id,
                    "agent_tune_arret_hors_delai_suppression_maintenue"
                );
            }
        }
        match repo.delete(id) {
            Ok(0) => {}
            Ok(_) => {
                tracing::info!(zone_id = id, device_id = %device_id, "agent_tune_zone_supprimee_appairage_revoque");
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::ZoneDeleted,
                    json!({ "id": id }),
                );
            }
            Err(e) => {
                tracing::warn!(zone_id = id, error = %e, "agent_tune_zone_non_supprimee");
            }
        }
    }
}

/// Retire l'agent de la liste des agents appairés et le rend.
fn retirer_de_la_liste(state: &AppState, agent_id: &str) -> Option<AgentAppaire> {
    let mut liste = agents(state);
    let position = liste.iter().position(|a| a.agent_id == agent_id)?;
    let agent = liste.remove(position);
    if let Err(e) = enregistrer_agents(state, &liste) {
        tracing::warn!(error = %e, "agent_tune_agent_non_retire");
        return None;
    }
    Some(agent)
}

/// Pourquoi les sorties d'un agent n'ont pas pu être lues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchecLecture {
    /// Pas de réponse, ou une réponse qui n'est pas un refus du jeton.
    Injoignable(String),
    /// L'agent répond, mais refuse le jeton (401) : révocation probable, à
    /// confirmer par [`revocation_confirmee`].
    JetonRefuse,
}

impl std::fmt::Display for EchecLecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Injoignable(motif) => f.write_str(motif),
            Self::JetonRefuse => f.write_str("jeton d'appairage refusé par l'agent"),
        }
    }
}

/// Lit les sorties d'un agent appairé.
pub async fn lire_sorties(agent: &AgentAppaire) -> Result<Vec<SortieExposee>, EchecLecture> {
    let reponse = client()
        .get(format!("{}/agent-tune/sorties", agent.base_url()))
        .header(ENTETE_JETON, &agent.jeton)
        .timeout(DELAI_SONDE)
        .send()
        .await
        .map_err(|e| EchecLecture::Injoignable(format!("agent injoignable : {e}")))?;
    if reponse.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(EchecLecture::JetonRefuse);
    }
    if !reponse.status().is_success() {
        return Err(EchecLecture::Injoignable(format!(
            "agent : {}",
            reponse.status()
        )));
    }
    reponse
        .json()
        .await
        .map_err(|e| EchecLecture::Injoignable(format!("réponse illisible : {e}")))
}

/// L'agent a-t-il VRAIMENT révoqué ce maître ? Il faut que le serveur joint à
/// son adresse refuse notre jeton ET qu'il soit bien cet agent : après un
/// changement d'adresse (DHCP), un autre serveur Tune peut répondre 401 à la
/// même adresse, et ses zones n'ont pas à en pâtir.
async fn revocation_confirmee(agent: &AgentAppaire) -> bool {
    if lire_sorties(agent).await != Err(EchecLecture::JetonRefuse) {
        return false;
    }
    sonder_annonce(&agent.host, agent.port)
        .await
        .and_then(|a| {
            a.get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|id| id == agent.agent_id)
}

/// L'appairage avec cet agent est révoqué : il quitte la liste, ses sorties
/// le registre, et ses zones sont supprimées.
async fn apres_revocation(state: &AppState, agent_id: &str) {
    if let Some(agent) = retirer_de_la_liste(state, agent_id) {
        supprimer_zones(state, agent_id).await;
        retirer_sorties(state, agent_id).await;
        tracing::info!(agent = %agent.nom, agent_id = %agent_id, "agent_tune_revocation_constatee");
    }
}

/// Un agent annonce qu'il a révoqué ce maître (`POST /agent-tune/revocation`).
/// L'annonce n'est pas crue sur parole : la révocation est vérifiée auprès de
/// l'agent. Rend `true` si elle est confirmée et appliquée.
pub async fn constater_revocation(state: &AppState, agent_id: &str) -> bool {
    let Some(agent) = agents(state).into_iter().find(|a| a.agent_id == agent_id) else {
        return false;
    };
    if !revocation_confirmee(&agent).await {
        tracing::info!(agent_id = %agent_id, "agent_tune_revocation_non_confirmee");
        return false;
    }
    apres_revocation(state, agent_id).await;
    true
}

/// Au démarrage : réinscrit les sorties de chaque agent joignable. Un agent
/// muet garde ses zones hors ligne ; un agent qui a révoqué ce maître pendant
/// son absence fait supprimer les siennes.
pub async fn reinscrire_les_agents(state: &AppState) {
    for agent in agents(state) {
        match lire_sorties(&agent).await {
            Ok(sorties) => {
                inscrire_sorties(state, &agent, &sorties).await;
            }
            Err(EchecLecture::JetonRefuse) if revocation_confirmee(&agent).await => {
                apres_revocation(state, &agent.agent_id).await;
            }
            Err(e) => {
                tracing::info!(agent = %agent.nom, error = %e, "agent_tune_agent_hors_ligne");
                retirer_sorties(state, &agent.agent_id).await;
            }
        }
    }
}

/// Oublie un agent (révocation côté maître) : ses zones sont supprimées, ses
/// sorties quittent le registre, et l'agent est prié (au mieux) d'oublier ce
/// maître.
pub async fn oublier(state: &AppState, agent_id: &str) -> bool {
    let Some(agent) = retirer_de_la_liste(state, agent_id) else {
        return false;
    };
    // Avant de retirer les sorties : l'arrêt d'une zone qui joue passe encore
    // par la sortie de l'agent, qui connaît toujours ce maître.
    supprimer_zones(state, agent_id).await;
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
