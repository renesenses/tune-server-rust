//! Fil 3075 — reprise d'une zone navigateur dont la session a expiré, quand
//! la piste elle-même n'est plus jouable.
//!
//! Depuis #8967, `resume` rétablit une zone navigateur à sa position par un
//! flux neuf. Quand ce rétablissement échoue parce que la PISTE a disparu
//! (ligne retirée de la bibliothèque, fichier supprimé, titre retiré par le
//! service), l'appelant recevait le même 502 « session perdue » qu'une panne
//! de sortie : rien ne permettait au client de distinguer « réessayez » de
//! « cette piste n'existe plus ».
//!
//! Cette panne-là porte désormais un code stable,
//! [`CODE_PISTE_DISPARUE_A_LA_REPRISE`], sur la réponse HTTP (410) et sur
//! l'événement `zone.playback_error`.

use super::PlaybackOrchestrator;
use crate::outputs::{OutputCommand, OutputCommandError};
use tracing::warn;

/// Code stable rendu quand la reprise est impossible parce que la piste a
/// disparu. Ne pas renommer : les clients s'y fient.
pub const CODE_PISTE_DISPARUE_A_LA_REPRISE: &str = "resume_track_unavailable";

/// L'échec de relecture dit-il que la PISTE n'est plus jouable (et non qu'une
/// sortie, un réseau ou un service est en panne) ?
pub(crate) fn piste_disparue(cause: &str) -> bool {
    cause == "track not found"
        || cause.starts_with("file_not_found:")
        || cause.contains("track has no file_path")
        || crate::poller::refus_propre_a_la_piste(cause)
}

/// Le message porté par l'erreur de commande : le code, puis la phrase dite à
/// l'auditeur. [`message_de_piste_disparue`] fait le chemin inverse.
fn avec_le_code(message: &str) -> String {
    format!("{CODE_PISTE_DISPARUE_A_LA_REPRISE}: {message}")
}

/// La phrase destinée à l'auditeur, quand `message` est celui d'une reprise
/// refusée pour piste disparue ; `None` pour toute autre erreur.
pub fn message_de_piste_disparue(message: &str) -> Option<&str> {
    message
        .strip_prefix(CODE_PISTE_DISPARUE_A_LA_REPRISE)
        .and_then(|reste| reste.strip_prefix(": "))
}

impl PlaybackOrchestrator {
    /// Jumeau de `dire_session_perdue` pour une piste qui n'existe plus : le
    /// même événement à tous les clients, mais avec le code
    /// [`CODE_PISTE_DISPARUE_A_LA_REPRISE`], et une erreur que la route
    /// reconnaît pour répondre 410 au lieu de 502.
    pub(super) fn dire_piste_disparue(
        &self,
        zone_id: i64,
        titre: &str,
        position_ms: Option<u64>,
        cause: &str,
        message: String,
    ) -> OutputCommandError {
        warn!(zone_id, ?position_ms, %cause, %message, "resume_track_unavailable");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message.clone(),
                    "code": CODE_PISTE_DISPARUE_A_LA_REPRISE,
                    "title": titre,
                    "position_ms": position_ms,
                    "fatal": true,
                }),
            );
        }
        OutputCommandError::failed(OutputCommand::Resume, avec_le_code(&message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_causes_de_piste_disparue_sont_reconnues() {
        assert!(piste_disparue("track not found"));
        assert!(piste_disparue("file_not_found:/musique/a.flac"));
        assert!(piste_disparue("streaming resolution failed: no url"));
        assert!(!piste_disparue("output dlna:x is not registered"));
        assert!(!piste_disparue("rien en lecture sur cette zone"));
    }

    #[test]
    fn le_message_se_relit_sans_le_code() {
        let m = avec_le_code("La piste « A » n'est plus disponible.");
        assert_eq!(
            message_de_piste_disparue(&m),
            Some("La piste « A » n'est plus disponible.")
        );
        assert_eq!(message_de_piste_disparue("autre erreur"), None);
    }
}
