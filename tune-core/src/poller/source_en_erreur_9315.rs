//! #9315 — la SOURCE d'un flux relayé a répondu par une erreur HTTP.
//!
//! Forum (relevé du 21/11) : un épisode de podcast Radio France en 404 chez
//! l'éditeur. Le mandataire relayait la page d'erreur au renderer comme de
//! l'audio ; la zone semblait démarrer, puis s'arrêtait 77 s plus tard sans
//! un mot.
//!
//! `tune-stream-http` rend désormais 502 au renderer et note le statut sur la
//! session (`StreamSession::signaler_erreur_amont`) tant que rien n'a été
//! servi. Le sondeur le lit à chaque tour : la zone que Tune tient en lecture
//! sur cette session s'arrête, avec un bandeau fatal (`zone.playback_error`,
//! code [`CODE_SOURCE_EN_ERREUR`], paramètres `status` et `title`). Aucune
//! relance : la source a refusé, rejouer la même adresse rendrait la même
//! erreur.

use crate::streaming::episodes_indisponibles;

/// Code stable du bandeau, que le client traduit (`erreursServeur.<code>`).
pub(super) const CODE_SOURCE_EN_ERREUR: &str = "source_http_error";

/// La charge de `zone.playback_error` pour une source en erreur `statut`.
///
/// #7077 — `error` n'est que le repli des anciens clients, en anglais : la
/// phrase de l'écran vient du code.
pub(super) fn charge_source_en_erreur(zone_id: i64, titre: &str, statut: u16) -> serde_json::Value {
    // Fil 2972 — 404/410 : le fichier de l'épisode n'existe plus chez
    // l'éditeur. Le code dit « épisode indisponible », que le client traduit.
    if episodes_indisponibles::statut_introuvable(statut) {
        return serde_json::json!({
            "zone_id": zone_id,
            "error": format!(
                "Episode unavailable at the publisher: \u{201c}{titre}\u{201d} (HTTP {statut}). The zone was stopped."
            ),
            "code": episodes_indisponibles::CODE_EPISODE_INDISPONIBLE,
            "params": { "status": statut, "title": titre },
            "fatal": true,
        });
    }
    serde_json::json!({
        "zone_id": zone_id,
        "error": format!(
            "The source refused \u{201c}{titre}\u{201d}: HTTP {statut}. The zone was stopped."
        ),
        "code": CODE_SOURCE_EN_ERREUR,
        "params": { "status": statut, "title": titre },
        "fatal": true,
    })
}

impl super::PositionPoller {
    /// Arrête la zone dont la source est en erreur, avec le bandeau.
    /// L'état de sondage a déjà été retiré par l'appelant.
    pub(super) async fn arreter_sur_source_en_erreur(
        &self,
        zone_id: i64,
        appareil: &str,
        titre: &str,
        adresse: Option<&str>,
        statut: u16,
    ) {
        // Fil 2972 — l'éditeur ne sert plus ce fichier : l'épisode est marqué
        // indisponible, les listes le diront (`unavailable: true`).
        if episodes_indisponibles::statut_introuvable(statut)
            && let Some(adresse) = adresse
        {
            episodes_indisponibles::marquer(adresse, statut);
        }
        tracing::warn!(
            zone_id,
            device = %appareil,
            statut,
            titre = %titre,
            "source_en_erreur_http_arret_9315"
        );
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                charge_source_en_erreur(zone_id, titre, statut),
            );
        }
        self.orchestrator.stop(zone_id, Some(appareil)).await;
    }
}
