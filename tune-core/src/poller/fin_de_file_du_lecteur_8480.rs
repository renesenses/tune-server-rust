//! #8480 — la lecture automatique d'une zone NAVIGATEUR en fin de file.
//!
//! ## La cause
//!
//! Une zone navigateur (le lecteur du client web, « Ce téléphone » des
//! applis) n'a pas de périphérique : le sondeur ne voit pas la fin de sa
//! piste. C'est le LECTEUR qui la dit, en appelant `POST /zones/{id}/next` à
//! l'évènement `ended` (`browserAudio.ts`). Sur la DERNIÈRE piste de la file,
//! cette route n'a pas de suivante et ARRÊTE la zone (`end_of_queue`) : la fin
//! de file du sondeur ([`PositionPoller::handle_track_end`] → lecture
//! automatique « titres similaires » ou aléatoire) n'est jamais jouée. Le
//! filet de #6932 (`fin_navigateur_6932`) n'y peut rien : il n'agit que si
//! PERSONNE n'a demandé la suite, et le lecteur l'a demandée.
//!
//! ## La règle
//!
//! Quand le `next` d'une zone navigateur tombe en fin de file ET que sa
//! piste est arrivée à son terme (la fin naturelle que le lecteur annonce),
//! la route passe la main à la fin de piste du sondeur, comme pour toute
//! autre zone : lecture automatique si la zone l'a demandée, arrêt sinon. Un
//! « Suivant » pressé au milieu de la dernière piste arrête toujours la zone.

use super::*;

/// Écart toléré entre la fin calculée et l'appel du lecteur : sa dernière
/// mesure de position (`browser-playing`) date de quelques secondes quand
/// `ended` arrive.
pub(crate) const TOLERANCE_FIN_DU_LECTEUR: std::time::Duration = std::time::Duration::from_secs(5);

/// La piste de cette zone est-elle arrivée à son terme, à `tolerance` près ?
///
/// Fonction pure, même ancre que [`super::fin_navigateur_6932::piste_navigateur_finie`] :
/// la dernière mesure du lecteur, sinon la commande qui a ouvert la fenêtre.
pub(crate) fn piste_au_terme(
    etat: &crate::playback::ZoneState,
    tolerance: std::time::Duration,
) -> bool {
    if etat.state != PlayState::Playing {
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
    let Some(ancre) = etat
        .derniere_avance_de_position
        .or(etat.observation_rouverte_a)
    else {
        return false;
    };
    let reste = std::time::Duration::from_millis(
        duree_ms.saturating_sub(etat.position_ms.max(0)).max(0) as u64,
    );
    ancre.elapsed() + tolerance >= reste
}

impl PositionPoller {
    /// #8480 — le `next` du lecteur d'une zone navigateur arrive en fin de
    /// file : si sa piste est à son terme, la fin de file du sondeur est jouée
    /// (lecture automatique ou arrêt). Rend `true` si la fin a été traitée ;
    /// `false` laisse la route faire comme avant.
    pub async fn fin_de_file_du_lecteur(&self, zone_id: i64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        if Self::next_position_manual(&etat).is_some()
            || !piste_au_terme(&etat, TOLERANCE_FIN_DU_LECTEUR)
        {
            return false;
        }
        let navigateur = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.output_type.as_deref() == Some("browser"));
        if !navigateur {
            return false;
        }
        info!(
            zone_id,
            queue_pos = etat.queue_position,
            queue_len = etat.queue_length,
            "fin_de_file_du_lecteur_navigateur"
        );
        // Fil 2694 (relecture de #8656) — la piste est allée à son terme : sur
        // une zone navigateur, le `next` du lecteur ne passe jamais par
        // `handle_track_end`, c'est donc ICI que le lot de la radio est noté
        // joué. Sans cela, la fin du premier lot était écartée
        // (`lot_precedent_sans_titre_joue`) et la zone s'arrêtait.
        self.noter_la_ligne_courante_jouee(zone_id, etat.queue_position);
        let device_id = self.get_zone_device_id(zone_id);
        self.terminer_la_file(zone_id, &etat, device_id.as_deref())
            .await;
        true
    }
}
