//! Ce que le greffon demande à l'hôte pour JOUER : poser une file sur une
//! zone et la lancer, arrêter une zone, savoir ce qu'une zone joue.
//!
//! Un trait, pour que les routes et la surveillance de l'éjection se prouvent
//! sans orchestrateur. L'implémentation de production (`HoteOrchestrateur`)
//! fait exactement ce que fait la route `POST /zones/{id}/play` (`routes/playback.rs`) pour un album de
//! service : `set_streaming_queue`, `update_queue_info`, puis
//! `orchestrator.play` sur la première ligne.

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::{PlayState, PlaybackManager};

use crate::fournisseur::SOURCE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementFile {
    pub source_id: String,
    pub titre: String,
    pub artiste: String,
    pub album: Option<String>,
    pub pochette: Option<String>,
    pub duree_ms: i64,
    pub numero: u8,
}

#[async_trait]
pub trait HoteLecture: Send + Sync {
    /// Remplace la file de la zone par `elements` et joue `elements[depart]`.
    async fn jouer_file(
        &self,
        zone_id: i64,
        elements: Vec<ElementFile>,
        depart: usize,
    ) -> Result<(), String>;
    /// Arrête la zone, comme le bouton Stop.
    async fn arreter(&self, zone_id: i64);
    /// La `source` de ce que la zone joue ou tient en pause, s'il y a lieu.
    async fn source_en_cours(&self, zone_id: i64) -> Option<String>;
    /// #6410 — la zone joue encore le disque, ou le tient en PAUSE. Une zone
    /// arrêtée (bouton Stop, fin de la file) garde sa ligne `now_playing`
    /// pour la reprise : `source_en_cours` ne suffit pas à dire qu'elle a
    /// fini.
    async fn lit_le_disque(&self, zone_id: i64) -> bool {
        self.source_en_cours(zone_id).await.as_deref() == Some(SOURCE)
    }
}

pub struct HoteOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
}

#[async_trait]
impl HoteLecture for HoteOrchestrateur {
    async fn jouer_file(
        &self,
        zone_id: i64,
        elements: Vec<ElementFile>,
        depart: usize,
    ) -> Result<(), String> {
        let premier = elements
            .get(depart)
            .cloned()
            .ok_or("piste de départ hors de la file")?;
        let lignes: Vec<_> = elements
            .iter()
            .map(|e| {
                (
                    e.source_id.clone(),
                    e.titre.clone(),
                    e.artiste.clone(),
                    e.album.clone(),
                    e.pochette.clone(),
                    e.duree_ms,
                    Some(SOURCE.to_string()),
                    Some(e.numero as i64),
                    Some(1),
                )
            })
            .collect();
        // La file AVANT la lecture, comme `POST /zones/{id}/play` : le client qui la
        // relit à l'annonce de la lecture doit la trouver.
        PlayQueueRepo::with_backend(self.backend.clone()).set_streaming_queue(zone_id, &lignes)?;
        let longueur = elements.len() as i64;
        self.playback
            .update_queue_info(zone_id, depart as i64, longueur)
            .await;
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
                source_id: Some(premier.source_id),
                title: Some(premier.titre),
                artist_name: Some(premier.artiste),
                album_title: premier.album,
                cover_url: premier.pochette,
                duration_ms: Some(premier.duree_ms),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                media_format: Some("wav".into()),
                track_number: Some(premier.numero as u32),
                disc_number: Some(1),
                album_ref: None,
            })
            .await?;
        // Réaffirmée APRÈS play(), pour la même raison que `POST /zones/{id}/play` :
        // sur une zone neuve, play() crée l'état avec une file de longueur 0.
        self.playback
            .update_queue_info(zone_id, depart as i64, longueur)
            .await;
        match resultat.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn arreter(&self, zone_id: i64) {
        self.orchestrator.stop(zone_id, None).await;
    }

    async fn source_en_cours(&self, zone_id: i64) -> Option<String> {
        let etat = self.playback.get_state(zone_id).await;
        // #7104 — Stop conserve now_playing pour la reprise, mais ne tient
        // plus le disque. Une pause le réserve encore, comme une lecture.
        if etat.state == tune_core::playback::PlayState::Stopped {
            return None;
        }
        etat.now_playing.map(|np| np.source)
    }

    async fn lit_le_disque(&self, zone_id: i64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        etat.state != PlayState::Stopped && etat.now_playing.is_some_and(|np| np.source == SOURCE)
    }
}
