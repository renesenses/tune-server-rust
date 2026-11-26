//! Fil 3094 (Xavier, rc13.90) — une file qui contient encore le balayage
//! d'une mesure de calibration finie (serveur redémarré pendant une mesure,
//! file d'une version antérieure) ne le rejoue pas : Lecture l'ÉCARTE et
//! passe à la ligne suivante, une fois, au lieu d'échouer à chaque appui sur
//! « aucune mesure de calibration armée ».
//!
//! Une ligne n'est écartée que si elle est TECHNIQUE et PÉRIMÉE
//! ([`crate::source_pcm::SourcesPcm::ligne_technique_perimee`]) : le balayage
//! d'une mesure en cours reste jouable, et aucune ligne de musique n'est
//! jamais touchée.

use super::*;
use crate::db::file_technique_3094::{Retrait, retirer_les_lignes_techniques};

/// Le refus d'une lecture directe d'un balayage périmé : une phrase, pas le
/// détail du fournisseur.
pub const BALAYAGE_PERIME: &str =
    "Ce balayage de calibration appartient à une mesure terminée : il a été retiré de la file.";

impl PlaybackOrchestrator {
    /// Écarte de la file de `zone_id` les lignes techniques périmées et
    /// replace le curseur. Rend ce qui a été retiré (rien, le plus souvent).
    pub async fn ecarter_les_balayages_perimes(&self, zone_id: i64) -> Retrait {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        // Le cas courant — aucune ligne technique — coûte une requête, pas la
        // lecture d'une file de mille titres à chaque changement de piste.
        if repo
            .count_de_la_source(zone_id, crate::source_pcm::SOURCE_CALIBRATION)
            .unwrap_or(1)
            == 0
        {
            self.oublier_le_balayage_affiche(zone_id).await;
            return Retrait::default();
        }
        let sources = self.sources_pcm();
        let retrait = match retirer_les_lignes_techniques(&repo, zone_id, |e| {
            sources.ligne_technique_perimee(
                e.source.as_deref().unwrap_or_default(),
                e.source_id.as_deref().unwrap_or_default(),
            )
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(zone_id, error = %e, "file_balayage_perime_non_retire");
                return Retrait::default();
            }
        };
        if !retrait.retirees.is_empty() {
            self.playback
                .update_queue_info(zone_id, retrait.courant.unwrap_or(0), retrait.longueur)
                .await;
            info!(
                zone_id,
                retirees = retrait.retirees.len(),
                longueur = retrait.longueur,
                "file_balayage_de_calibration_perime_retire"
            );
            if let Some(ref bus) = self.event_bus {
                for (position, titre) in retrait.retirees.iter().zip(&retrait.titres) {
                    bus.emit(
                        "playback.track_skipped",
                        serde_json::json!({
                            "zone_id": zone_id,
                            "position": position,
                            "title": titre,
                            "reason": "calibration_terminee",
                        }),
                    );
                }
            }
        }
        self.oublier_le_balayage_affiche(zone_id).await;
        retrait
    }

    /// Une zone à l'arrêt qui AFFICHE encore un balayage périmé l'oublie :
    /// sinon le client renvoie ce titre à chaque appui sur Lecture.
    pub async fn oublier_le_balayage_affiche(&self, zone_id: i64) {
        let etat = self.playback.get_state(zone_id).await;
        let perime = etat.state != PlayState::Playing
            && etat.now_playing.as_ref().is_some_and(|np| {
                self.sources_pcm().ligne_technique_perimee(
                    &np.source,
                    np.source_id.as_deref().unwrap_or_default(),
                )
            });
        if perime {
            self.playback.stop_and_clear(zone_id).await;
        }
    }

    /// La demande vise-t-elle un balayage périmé ? Alors la file en est
    /// purgée et la demande refusée en une phrase.
    pub(super) async fn refuser_un_balayage_perime(&self, req: &PlayRequest) -> Option<String> {
        let source = req.source.as_deref()?;
        let source_id = req.source_id.as_deref().unwrap_or_default();
        if !self
            .sources_pcm()
            .ligne_technique_perimee(source, source_id)
        {
            return None;
        }
        warn!(
            zone_id = req.zone_id,
            source, source_id, "lecture_d_un_balayage_de_calibration_perime_refusee"
        );
        self.ecarter_les_balayages_perimes(req.zone_id).await;
        Some(BALAYAGE_PERIME.to_string())
    }
}

#[cfg(test)]
#[path = "file_technique_3094_tests.rs"]
mod tests;
