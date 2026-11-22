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

/// Code stable du bandeau, que le client traduit (`erreursServeur.<code>`).
pub(super) const CODE_SOURCE_EN_ERREUR: &str = "source_http_error";

/// La charge de `zone.playback_error` pour une source en erreur `statut`.
///
/// #7077 — `error` n'est que le repli des anciens clients, en anglais : la
/// phrase de l'écran vient du code.
pub(super) fn charge_source_en_erreur(zone_id: i64, titre: &str, statut: u16) -> serde_json::Value {
    let precision = match statut {
        404 | 410 => " (not found at the publisher)",
        _ => "",
    };
    serde_json::json!({
        "zone_id": zone_id,
        "error": format!(
            "The source refused \u{201c}{titre}\u{201d}: HTTP {statut}{precision}. The zone was stopped."
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
        statut: u16,
    ) {
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
