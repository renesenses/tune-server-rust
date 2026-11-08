//! #8152 — un épisode de podcast repris sur un renderer réseau retrouve sa
//! position.
//!
//! Constat du .18 le 08/11 (Eversolo DMP-A8 en DLNA, « Certains l'aiment
//! Fip ») : `play_episode` envoyait le `Seek` de reprise (556 s) 2 ms après
//! l'acquittement du `Play`, à 14:39:01.743. Le renderer l'a acquitté, puis a
//! ouvert le flux à 14:39:02.435 en `bytes=0-` : l'épisode repartait du début
//! pendant que Tune affichait 9:16. C'est le défaut du fil 2125 (#5711) : un
//! `Seek` parti avant que le renderer ait ouvert le flux est perdu. La pose
//! fixe des autres reprises (`REPLAY_OUTPUT_SEEK_SETTLE_MS`, 500 ms) n'y
//! suffisait pas non plus : 700 ms séparaient ici le `Play` de l'ouverture.
//!
//! Sur une sortie réseau dont la session sait chercher (mandataire de
//! l'épisode, #6468), le `Seek` part donc d'une tâche détachée qui attend que
//! le renderer se dise EN LECTURE, avec les gardes des reprises existantes :
//! génération des commandes de transport (un geste de l'utilisateur rend la
//! tâche caduque, #5476) et relance d'un renderer resté en pause après le
//! `Seek` (Rygel, fil 1780). Ailleurs (sortie locale, session sans `Range`),
//! [`PlaybackOrchestrator::seek`] fait ce qu'il faisait déjà.
use super::PlaybackOrchestrator;
use super::session::{replay_needs_output_seek, reprise_toujours_la_notre};
use crate::outputs::{OutputCommandResult, TransportState};
use crate::playback::PlaybackManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Intervalle entre deux lectures de l'état du renderer.
const SONDE_DE_LECTURE: Duration = Duration::from_millis(250);
/// Au-delà, le `Seek` part quand même : un renderer qui ne remonte jamais
/// `PLAYING` (état figé, évènements perdus) garde sa chance de sauter.
const ATTENTE_MAX_DE_LECTURE: Duration = Duration::from_secs(15);

impl PlaybackOrchestrator {
    /// Ramener l'épisode qui vient de partir vers `device_id` à
    /// `position_ms`. Position nulle : rien.
    ///
    /// Sur une sortie réseau à session cherchable, le `Ok` ne prouve pas que
    /// le renderer a sauté : la tâche détachée attend sa lecture. La position
    /// publique, elle, suit tout de suite (l'interface ne repart pas de 0:00
    /// et la reprise enregistrée n'est pas écrasée par 0).
    pub async fn reprendre_l_episode_a_sa_position(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
        position_ms: u64,
    ) -> OutputCommandResult<()> {
        if position_ms == 0 {
            return Ok(());
        }
        let Some(did) = device_id else {
            return self.seek(zone_id, position_ms, None).await;
        };
        let output_type = self.output_type_of(did).await;
        let stream_id = self
            .playback
            .get_state(zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id);
        let cherchable = match stream_id {
            Some(ref sid) => self.streamer.is_seekable_session(sid).await,
            None => false,
        };
        if !replay_needs_output_seek(
            super::is_network_output_type(output_type.as_deref()),
            cherchable,
            position_ms,
        ) {
            return self.seek(zone_id, position_ms, Some(did)).await;
        }
        self.playback.seek(zone_id, position_ms as i64).await;
        let seq_au_depart = self.playback.current_transport_seq(zone_id).await;
        let outputs = self.outputs.clone();
        let playback = self.playback.clone();
        let did = did.to_string();
        tokio::spawn(async move {
            seek_d_episode_une_fois_en_lecture(
                outputs,
                playback,
                zone_id,
                did,
                position_ms,
                seq_au_depart,
                ATTENTE_MAX_DE_LECTURE,
            )
            .await;
        });
        Ok(())
    }
}

/// La tâche détachée : attendre `PLAYING`, puis `Seek`, puis relancer un
/// renderer resté en pause.
async fn seek_d_episode_une_fois_en_lecture(
    outputs: Arc<Mutex<crate::outputs::registry::OutputRegistry>>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    device_id: String,
    position_ms: u64,
    seq_au_depart: u64,
    attente_max: Duration,
) {
    let debut = std::time::Instant::now();
    let mut dernier_etat = None;
    loop {
        let seq_courante = playback.current_transport_seq(zone_id).await;
        if !reprise_toujours_la_notre(seq_au_depart, seq_courante) {
            info!(
                zone_id,
                position_ms, seq_au_depart, seq_courante, "podcast_reprise_position_abandonnee"
            );
            return;
        }
        let Some(output) = ({ outputs.lock().await.get(&device_id) }) else {
            warn!(zone_id, device_id = %device_id, "podcast_reprise_position_sortie_disparue");
            return;
        };
        let etat = output.lock().await.get_status().await.ok().map(|s| s.state);
        dernier_etat = etat;
        if etat == Some(TransportState::Playing) {
            break;
        }
        if debut.elapsed() >= attente_max {
            warn!(
                zone_id,
                position_ms,
                etat = ?etat,
                attente_ms = debut.elapsed().as_millis() as u64,
                "podcast_reprise_position_sans_lecture_confirmee"
            );
            break;
        }
        tokio::time::sleep(SONDE_DE_LECTURE).await;
    }
    let Some(output) = ({ outputs.lock().await.get(&device_id) }) else {
        return;
    };
    let sortie = output.lock().await;
    // Seconde lecture, sortie en main : un geste a pu attendre ce verrou.
    let seq_courante = playback.current_transport_seq(zone_id).await;
    if !reprise_toujours_la_notre(seq_au_depart, seq_courante) {
        info!(
            zone_id,
            position_ms, seq_au_depart, seq_courante, "podcast_reprise_position_abandonnee"
        );
        return;
    }
    match sortie.checked_seek(position_ms).await {
        Ok(()) => {
            info!(
                zone_id,
                position_ms,
                etat_avant = ?dernier_etat,
                attente_ms = debut.elapsed().as_millis() as u64,
                "podcast_reprise_position_envoyee"
            );
            playback.seek(zone_id, position_ms as i64).await;
            if let Ok(statut) = sortie.get_status().await
                && statut.state == TransportState::Paused
            {
                match sortie.checked_resume().await {
                    Ok(()) => info!(
                        zone_id,
                        position_ms, "podcast_reprise_position_relance_play_renderer_en_pause"
                    ),
                    Err(e) => warn!(
                        zone_id,
                        position_ms,
                        error = %e,
                        "podcast_reprise_position_relance_play_echouee"
                    ),
                }
            }
        }
        Err(e) => warn!(zone_id, position_ms, error = %e, "podcast_reprise_position_refusee"),
    }
}
