//! Côté MAÎTRE : la sortie qui représente une sortie d'un agent (#4626).
//!
//! Chaque appel de l'orchestrateur du maître devient un ordre HTTP vers
//! l'agent, porteur du jeton d'appairage. Le flux, lui, ne passe pas par ici :
//! l'agent va le chercher à l'URL que l'orchestrateur a construite.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// #6370 — la position dans la PISTE où commence le flux que l'agent lit.
    /// Sa sortie locale compte depuis le début du flux reçu ; un flux relancé
    /// à 2:00 par un déplacement y commence à 0. Remis à zéro à chaque
    /// lecture, posé par [`OutputTarget::seek`].
    depart_ms: AtomicU64,
    peripherique_rendu_en_pause: AtomicBool,
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
            depart_ms: AtomicU64::new(0),
            peripherique_rendu_en_pause: AtomicBool::new(false),
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
        let etat = reponse
            .json::<EtatSortieDistante>()
            .await
            .map_err(|e| format!("réponse d'agent illisible : {e}"))?;
        self.peripherique_rendu_en_pause.store(
            etat.tenue && etat.peripherique_rendu_en_pause,
            Ordering::SeqCst,
        );
        Ok(etat)
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
        // Un flux neuf part du début de la piste, sauf si l'orchestrateur dit
        // ensuite le contraire (`seek`, juste après une relance à la position).
        self.depart_ms.store(0, Ordering::SeqCst);
        self.peripherique_rendu_en_pause
            .store(false, Ordering::SeqCst);
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

    fn device_released_on_pause(&self) -> bool {
        self.peripherique_rendu_en_pause.load(Ordering::SeqCst)
    }

    async fn resume(&self) -> Result<(), String> {
        // #8825 — relire même sans passage du poller après la pause.
        // Le refus avec le drapeau armé emprunte le rétablissement existant
        // de l'orchestrateur (#6062), qui recrée les OCTETS à la position.
        // Rejouer simplement l'URL du bail repartirait au début du flux.
        self.lire_etat().await?;
        if self.device_released_on_pause() {
            return Err("agent Tune : périphérique rendu en pause, flux à rétablir".into());
        }
        self.envoyer(Commande::Reprendre).await
    }

    async fn stop(&self) -> Result<(), String> {
        self.envoyer(Commande::Arreter).await
    }

    /// Le contrat de `LocalOutput::seek` : NOTER la position où commence le
    /// flux relancé, sans rien envoyer.
    ///
    /// #6370 — l'agent ne prête que sa sortie locale, qui ne sait pas se
    /// déplacer dans un flux séquentiel : `Commande::Position` y notait la
    /// position et le son restait où il était (fil 1867). Le maître relance
    /// donc le flux à la position (`PlaybackOrchestrator::seek`, comme pour sa
    /// propre sortie locale) puis appelle ce `seek` ; l'agent, lui, ne reçoit
    /// qu'un `Lire` ordinaire — aucun agent installé n'a à changer.
    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.depart_ms.store(position_ms, Ordering::SeqCst);
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.envoyer(Commande::Volume { volume }).await
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.envoyer(Commande::Muet { muet: muted }).await
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let mut statut = self.lire_etat().await?.statut;
        statut.position_ms = statut
            .position_ms
            .saturating_add(self.depart_ms.load(Ordering::SeqCst));
        Ok(statut)
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
