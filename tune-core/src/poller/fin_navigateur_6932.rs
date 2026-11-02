//! #6932 — une zone NAVIGATEUR dont la piste est finie enchaîne, même si son
//! lecteur ne l'a pas dit.
//!
//! ## La scène (fil 2572, rc9, appli Android 1.0.30, zone « Ce téléphone »)
//!
//! ```text
//! 09:58:33.485 browser_playback_confirmed_announcing zone_id=14 title=01 - Prelude No. 1 …
//! 10:00:28.615 stream_connexion_terminee … octets_envoyes=100101164 … fin="non_suivie"
//! 10:02:47.608 api_next_requested zone_id=14
//! ```
//!
//! Une piste de 2 min 22 s, servie jusqu'au bout, puis RIEN pendant plus de
//! deux minutes : c'est l'utilisateur qui a fini par appuyer sur « suivant ».
//!
//! Une zone navigateur n'a pas de périphérique : le sondeur ne voit ni sa
//! position ni sa fin (`tick.rs` passe par la branche « sans périphérique »).
//! La suite dépend entièrement du LECTEUR — l'évènement `ended` du client web,
//! `ProcessingState.completed` de l'appli (#7149). Que ce signal manque (un
//! rappel jamais branché dans l'appli 1.0.30, un téléphone qui perd le
//! réseau à la fin du morceau) et la file s'arrête là, la zone annoncée « en
//! lecture » sur une piste finie.
//!
//! ## La règle
//!
//! Le serveur sait, sans rien supposer, quand la piste est finie :
//!
//! 1. le flux a été **servi jusqu'au bout** au lecteur
//!    ([`crate::http::streamer::AudioStreamer::stream_servi_jusqu_au_bout`]) —
//!    sans quoi le lecteur peut encore attendre des octets ;
//! 2. la **durée** de la piste est connue ;
//! 3. depuis la dernière mesure du lecteur (`browser-playing`, qui pose
//!    `derniere_avance_de_position` et `position_ms`) ou, à défaut, depuis la
//!    commande qui a ouvert la fenêtre (`play`, `resume`, `seek` :
//!    `observation_rouverte_a`), il s'est écoulé au moins
//!    `durée − position + MARGE_FIN_NAVIGATEUR`.
//!
//! Alors la piste est finie depuis la marge, et personne n'a demandé la
//! suite : le sondeur fait ce qu'il fait pour toute autre zone à la fin d'une
//! piste ([`PositionPoller::handle_track_end`] : répétition, aléatoire, fin
//! de file, autoplay).
//!
//! Un lecteur qui fait son travail passe toujours AVANT : son `next` arrive à
//! la fin du morceau, et le `play()` de la piste suivante rouvre la fenêtre.
//! La marge est là pour lui laisser cette priorité. Une PAUSE n'est jamais
//! concernée (`Playing` seulement), et une reprise rouvre la fenêtre.

use super::*;

/// Le temps laissé au lecteur pour dire lui-même que la piste est finie.
pub(crate) const MARGE_FIN_NAVIGATEUR: std::time::Duration = std::time::Duration::from_secs(20);

/// La piste de cette zone navigateur est-elle finie depuis au moins `marge`,
/// sans que personne ait demandé la suite ?
///
/// Fonction pure : `servi_jusqu_au_bout` est lu par l'appelant.
pub(crate) fn piste_navigateur_finie(
    etat: &crate::playback::ZoneState,
    servi_jusqu_au_bout: bool,
    marge: std::time::Duration,
) -> bool {
    if etat.state != PlayState::Playing || !servi_jusqu_au_bout {
        return false;
    }
    let Some(duree_ms) = etat
        .now_playing
        .as_ref()
        .filter(|np| np.source != "radio")
        .map(|np| np.duration_ms)
        .filter(|d| *d > 0)
    else {
        return false;
    };
    // La dernière mesure du lecteur, sinon la commande qui a ouvert la
    // fenêtre. Ni l'une ni l'autre (état restauré) : on ne conclut rien.
    let Some(ancre) = etat
        .derniere_avance_de_position
        .or(etat.observation_rouverte_a)
    else {
        return false;
    };
    let reste_ms = duree_ms.saturating_sub(etat.position_ms.max(0)).max(0) as u64;
    ancre.elapsed() >= std::time::Duration::from_millis(reste_ms) + marge
}

impl PositionPoller {
    /// Branche « sans périphérique » du tick : enchaîne une zone navigateur
    /// dont la piste est finie (voir le module). Rend `true` si la fin a été
    /// traitée.
    pub(super) async fn enchainer_la_zone_navigateur_finie(
        &self,
        zone_state: &crate::playback::ZoneState,
    ) -> bool {
        let zone_id = zone_state.zone_id;
        let Some(stream_id) = zone_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        else {
            return false;
        };
        // Bon marché d'abord : sans durée écoulée, rien à lire de plus.
        if !piste_navigateur_finie(zone_state, true, MARGE_FIN_NAVIGATEUR) {
            return false;
        }
        let navigateur = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.output_type.as_deref() == Some("browser"));
        if !navigateur
            || !self
                .orchestrator
                .streamer_servi_jusqu_au_bout(stream_id)
                .await
        {
            return false;
        }
        warn!(
            zone_id,
            stream_id,
            title = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.title.as_str())
                .unwrap_or(""),
            duration_ms = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.duration_ms)
                .unwrap_or(0),
            position_ms = zone_state.position_ms,
            mesure_du_lecteur = zone_state.derniere_avance_de_position.is_some(),
            marge_secs = MARGE_FIN_NAVIGATEUR.as_secs(),
            "fin_navigateur_sans_signal_enchainee"
        );
        self.handle_track_end(zone_id, zone_state).await;
        true
    }
}

#[cfg(test)]
#[path = "fin_navigateur_6932_tests.rs"]
mod tests;
