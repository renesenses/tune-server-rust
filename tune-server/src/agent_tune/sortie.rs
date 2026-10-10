//! Côté MAÎTRE : la sortie qui représente une sortie d'un agent (#4626).
//!
//! Chaque appel de l'orchestrateur du maître devient un ordre HTTP vers
//! l'agent, porteur du jeton d'appairage. Le flux, lui, ne passe pas par ici :
//! l'agent va le chercher à l'URL que l'orchestrateur a construite.

use std::time::Duration;

use tune_core::outputs::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia};

use super::maitre::AgentAppaire;
use super::{
    Commande, DemandeEtat, ENTETE_JETON, EtatSortieDistante, OrdreSortie, PREFIXE_DEVICE_ID,
    SortieExposee, TYPE_DE_SORTIE,
};

/// Délai d'un ordre : l'agent ouvre sa sortie avant de répondre à `lire`.
const DELAI_ORDRE: Duration = Duration::from_secs(10);
/// Délai d'une lecture d'état ou d'une sonde de présence.
const DELAI_ETAT: Duration = Duration::from_secs(3);

/// Le `device_id` qu'une sortie d'agent porte chez le maître.
pub fn device_id_maitre(agent_id: &str, device_id_agent: &str) -> String {
    format!("{PREFIXE_DEVICE_ID}{agent_id}:{device_id_agent}")
}

/// Le nom de la zone chez le maître : la sortie, puis le serveur qui la porte.
pub fn nom_de_zone(sortie: &str, agent_nom: &str) -> String {
    format!("{sortie} — {agent_nom}")
}

pub struct SortieAgentTune {
    nom: String,
    device_id: String,
    device_id_agent: String,
    hote: String,
    base: String,
    jeton: String,
    capacites: OutputCapabilities,
    client: reqwest::Client,
}

impl SortieAgentTune {
    pub fn new(agent: &AgentAppaire, sortie: &SortieExposee) -> Self {
        let mut capacites = sortie.capacites.clone();
        // L'enchaînement interne n'est pas relayé dans cette première
        // version : le sondeur du maître avance la file en fin de piste.
        capacites.can_gapless = false;
        Self {
            nom: nom_de_zone(&sortie.nom, &agent.nom),
            device_id: device_id_maitre(&agent.agent_id, &sortie.device_id),
            device_id_agent: sortie.device_id.clone(),
            hote: agent.host.clone(),
            base: agent.base_url(),
            jeton: agent.jeton.clone(),
            capacites,
            client: tune_core::http::client::builder()
                .build()
                .unwrap_or_default(),
        }
    }

    async fn envoyer(&self, commande: Commande) -> Result<(), String> {
        let ordre = OrdreSortie {
            device_id: self.device_id_agent.clone(),
            commande,
        };
        let reponse = self
            .client
            .post(format!("{}/agent-tune/sorties/commande", self.base))
            .header(ENTETE_JETON, &self.jeton)
            .timeout(DELAI_ORDRE)
            .json(&ordre)
            .send()
            .await
            .map_err(|e| format!("agent Tune injoignable : {e}"))?;
        if reponse.status().is_success() {
            return Ok(());
        }
        let code = reponse.status();
        let motif = reponse.text().await.unwrap_or_default();
        Err(format!("agent Tune ({code}) : {motif}"))
    }

    async fn lire_etat(&self) -> Result<EtatSortieDistante, String> {
        let reponse = self
            .client
            .post(format!("{}/agent-tune/sorties/etat", self.base))
            .header(ENTETE_JETON, &self.jeton)
            .timeout(DELAI_ETAT)
            .json(&DemandeEtat {
                device_id: self.device_id_agent.clone(),
            })
            .send()
            .await
            .map_err(|e| format!("agent Tune injoignable : {e}"))?;
        if !reponse.status().is_success() {
            let code = reponse.status();
            let motif = reponse.text().await.unwrap_or_default();
            return Err(format!("agent Tune ({code}) : {motif}"));
        }
        reponse
            .json::<EtatSortieDistante>()
            .await
            .map_err(|e| format!("réponse d'agent illisible : {e}"))
    }
}

#[async_trait::async_trait]
impl OutputTarget for SortieAgentTune {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn name(&self) -> &str {
        &self.nom
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        TYPE_DE_SORTIE
    }

    fn capabilities(&self) -> OutputCapabilities {
        self.capacites.clone()
    }

    fn supports_internal_gapless(&self) -> bool {
        false
    }

    fn host(&self) -> Option<&str> {
        Some(&self.hote)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.envoyer(Commande::Lire {
            url: media.url.to_string(),
            mime_type: media.mime_type.to_string(),
            titre: media.title.map(str::to_string),
            artiste: media.artist.map(str::to_string),
            album: media.album.map(str::to_string),
            pochette: media.cover_url.map(str::to_string),
            duree_ms: media.duration_ms,
            direct: media.live_stream,
        })
        .await
    }

    async fn pause(&self) -> Result<(), String> {
        self.envoyer(Commande::Pause).await
    }

    async fn resume(&self) -> Result<(), String> {
        self.envoyer(Commande::Reprendre).await
    }

    async fn stop(&self) -> Result<(), String> {
        self.envoyer(Commande::Arreter).await
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.envoyer(Commande::Position { position_ms }).await
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.envoyer(Commande::Volume { volume }).await
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.envoyer(Commande::Muet { muet: muted }).await
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        self.lire_etat().await.map(|e| e.statut)
    }

    async fn is_available(&self) -> bool {
        self.lire_etat().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_device_id_maitre_garde_l_agent_et_la_sortie() {
        assert_eq!(
            device_id_maitre("abc", "local:hw:1,0"),
            "tune-agent:abc:local:hw:1,0"
        );
    }
}
