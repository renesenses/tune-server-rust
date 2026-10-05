//! Tune Circle, étape T5 (#5328) : jouer une playlist de cercle résolue.
//!
//! Un trait, pour que la route se prouve sans orchestrateur, comme
//! `tune-cd` (`hote.rs`). L'implémentation de production fait ce que fait
//! une file posée par le serveur : la file de la zone est remplacée
//! (`clear`, puis `insert_at_bilan`, qui mêle pistes locales et titres de
//! service), la tête devient courante, puis `orchestrator.play` sur elle.

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::PlaybackManager;

#[async_trait]
pub trait Lecture: Send + Sync {
    /// Remplace la file de la zone par `elements` et joue le premier.
    /// Rend le nombre de lignes réellement entrées dans la file.
    async fn jouer(&self, zone_id: i64, elements: Vec<QueueInput>) -> Result<usize, String>;
}

pub struct LectureOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
}

fn requete_de_lecture(
    zone_id: i64,
    output_device_id: Option<String>,
    e: &QueueInput,
) -> PlayRequest {
    match e {
        QueueInput::Local { track_id } => PlayRequest {
            zone_id,
            output_device_id,
            track_id: Some(*track_id),
            ..Default::default()
        },
        QueueInput::Streaming {
            source,
            source_id,
            title,
            artist,
            album,
            cover_url,
            duration_ms,
            track_number,
            disc_number,
            album_ref,
        } => PlayRequest {
            zone_id,
            output_device_id,
            source: Some(source.clone()),
            source_id: Some(source_id.clone()),
            title: Some(title.clone()),
            artist_name: Some(artist.clone()),
            album_title: album.clone(),
            cover_url: cover_url.clone(),
            duration_ms: Some(*duration_ms),
            track_number: track_number.map(|n| n as u32),
            disc_number: disc_number.map(|n| n as u32),
            album_ref: album_ref.clone(),
            ..Default::default()
        },
    }
}

#[async_trait]
impl Lecture for LectureOrchestrateur {
    async fn jouer(&self, zone_id: i64, elements: Vec<QueueInput>) -> Result<usize, String> {
        let zone = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("zone {zone_id} introuvable"))?;
        let file = PlayQueueRepo::with_backend(self.backend.clone());
        // La file AVANT la lecture : le client qui la relit à l'annonce de la
        // lecture doit la trouver.
        file.clear(zone_id)?;
        let bilan = file.insert_at_bilan(zone_id, &elements, None)?;
        let entrees = bilan.inserted();
        if entrees == 0 {
            return Err("aucune piste n'est entrée dans la file".into());
        }
        file.set_current_pos(zone_id, 0)?;
        let tete = file
            .get_ordered(zone_id)?
            .into_iter()
            .next()
            .ok_or("file vide après insertion")?;
        // La tête RÉELLE de la file : une piste locale disparue entre la
        // résolution et l'insertion a été sautée.
        let premier = elements
            .iter()
            .find(|e| match (e, tete.track_id) {
                (QueueInput::Local { track_id }, Some(t)) => *track_id == t,
                (QueueInput::Streaming { source_id, .. }, None) => {
                    tete.source_id.as_deref() == Some(source_id.as_str())
                }
                _ => false,
            })
            .unwrap_or(&elements[0]);
        let longueur = entrees as i64;
        self.playback.update_queue_info(zone_id, 0, longueur).await;
        let resultat = self
            .orchestrator
            .play(requete_de_lecture(zone_id, zone.output_device_id, premier))
            .await?;
        // Réaffirmée APRÈS play() : sur une zone neuve, play() crée l'état
        // avec une file de longueur 0 (même raison que `tune-cd`).
        self.playback.update_queue_info(zone_id, 0, longueur).await;
        match resultat.error {
            Some(e) => Err(e),
            None => Ok(entrees),
        }
    }
}
