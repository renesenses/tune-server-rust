//! Ce que le greffon demande à l'hôte pour JOUER : poser l'entrée en file sur
//! une zone et la lancer, arrêter une zone.
//!
//! Un trait, pour que les routes et le contrôleur se prouvent sans
//! orchestrateur. L'implémentation de production fait ce que fait
//! `POST /zones/{id}/play` pour un élément de service : `set_streaming_queue`,
//! `update_queue_info`, puis `orchestrator.play` — comme le greffon `cd`.

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::PlaybackManager;
use tune_core::source_pcm::FormatPcm;

use crate::fournisseur::SOURCE;

/// L'unique élément de file d'une entrée en direct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementDirect {
    /// Le nom du périphérique : c'est le `source_id` de la ligne.
    pub entree: String,
    pub format: FormatPcm,
}

impl ElementDirect {
    /// « Entrée audio — <périphérique> ».
    pub fn titre(&self) -> String {
        titre(&self.entree)
    }
}

pub fn titre(entree: &str) -> String {
    format!("Entrée audio — {entree}")
}

#[async_trait]
pub trait HoteLecture: Send + Sync {
    /// Remplace la file de la zone par l'entrée et la joue.
    async fn jouer(&self, zone_id: i64, element: ElementDirect) -> Result<(), String>;
    /// Arrête la zone, comme le bouton Stop.
    async fn arreter(&self, zone_id: i64);
    /// La `source` de ce que la zone joue, s'il y a lieu.
    async fn source_en_cours(&self, zone_id: i64) -> Option<String>;
    /// La position JOUÉE par la zone, si elle joue l'entrée en direct.
    async fn position_ms(&self, zone_id: i64) -> Option<i64>;
}

pub struct HoteOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
}

#[async_trait]
impl HoteLecture for HoteOrchestrateur {
    async fn jouer(&self, zone_id: i64, e: ElementDirect) -> Result<(), String> {
        let titre = e.titre();
        let ligne = (
            e.entree.clone(),
            titre.clone(),
            String::new(),
            None,
            None,
            // En direct : pas de durée.
            0,
            Some(SOURCE.to_string()),
            None,
            None,
        );
        PlayQueueRepo::with_backend(self.backend.clone()).set_streaming_queue(zone_id, &[ligne])?;
        self.playback.update_queue_info(zone_id, 0, 1).await;
        let output_device_id = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let resultat = self
            .orchestrator
            .play(PlayRequest {
                zone_id,
                output_device_id,
                track_id: None,
                source: Some(SOURCE.into()),
                source_id: Some(e.entree.clone()),
                title: Some(titre),
                artist_name: None,
                album_title: None,
                cover_url: None,
                duration_ms: None,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: Some(e.format.frequence),
                bit_depth: Some(e.format.bits),
                // Du PCM servi en WAV : sans perte.
                media_format: Some("wav".into()),
                track_number: None,
                disc_number: None,
            })
            .await?;
        self.playback.update_queue_info(zone_id, 0, 1).await;
        match resultat.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn arreter(&self, zone_id: i64) {
        self.orchestrator.stop(zone_id, None).await;
    }

    async fn source_en_cours(&self, zone_id: i64) -> Option<String> {
        self.playback
            .get_state(zone_id)
            .await
            .now_playing
            .map(|np| np.source)
    }

    async fn position_ms(&self, zone_id: i64) -> Option<i64> {
        let etat = self.playback.get_state(zone_id).await;
        let joue = etat.state == tune_core::playback::PlayState::Playing
            && etat.now_playing.is_some_and(|np| np.source == SOURCE);
        joue.then_some(etat.position_ms)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Un hôte qui retient ce qu'on lui demande.
    #[derive(Default)]
    pub struct HoteTemoin {
        pub joues: tokio::sync::Mutex<Vec<(i64, ElementDirect)>>,
        pub arretees: tokio::sync::Mutex<Vec<i64>>,
    }

    #[async_trait]
    impl HoteLecture for HoteTemoin {
        async fn jouer(&self, zone_id: i64, element: ElementDirect) -> Result<(), String> {
            self.joues.lock().await.push((zone_id, element));
            Ok(())
        }
        async fn arreter(&self, zone_id: i64) {
            self.arretees.lock().await.push(zone_id);
        }
        async fn source_en_cours(&self, zone_id: i64) -> Option<String> {
            let jouee = self.joues.lock().await.iter().any(|(z, _)| *z == zone_id);
            let arretee = self.arretees.lock().await.contains(&zone_id);
            (jouee && !arretee).then(|| SOURCE.to_string())
        }
        async fn position_ms(&self, _: i64) -> Option<i64> {
            None
        }
    }

    #[test]
    fn le_titre_nomme_le_peripherique() {
        assert_eq!(titre("Yeti X"), "Entrée audio — Yeti X");
    }
}
