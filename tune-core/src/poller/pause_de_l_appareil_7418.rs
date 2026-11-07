//! #7418 — une pause faite SUR L'APPAREIL n'est plus suivie d'une relance.
//!
//! Constat de r5yk5 (fil 2531, WiiM Ultra et Pro Plus en DLNA, rc8.1) : après
//! une pause à la télécommande WiiM ou un changement d'entrée, Tune affiche
//! toujours la lecture, puis la musique repart seule, parfois 10 à 15 minutes
//! plus tard.
//!
//! ## La cause
//!
//! Le bras `TransportState::Paused` du sondeur (`tick.rs`) remettait les
//! compteurs à zéro et rien d'autre : la zone restait `Playing` pendant que
//! l'appareil était en pause. Toute la surveillance d'une zone EN LECTURE
//! continuait donc de tourner sur un appareil que l'auditeur avait arrêté.
//! Quand l'appareil passe ensuite `Stopped` (autre entrée, téléviseur éteint,
//! flux abandonné après une longue pause), le bras `Stopped` lit cet arrêt
//! comme une fin de piste ou un renderer qui cale, et ses remèdes RELANCENT :
//! fin naturelle sous répétition (`natural_end`, `repeat_end` dès 5 s jouées),
//! fin de piste quand la pause tombait dans les derniers 20 %, relance d'une
//! radio, enchaînement « bloqué » forcé. La relance envoie une nouvelle
//! lecture à l'appareil, et un WiiM qui reçoit une lecture DLNA revient sur
//! son entrée réseau : c'est la musique qui repart.
//!
//! ## Le remède
//!
//! - Une sortie DLNA qui reste `Paused` pendant [`TOURS_DE_PAUSE_DE_L_APPAREIL`]
//!   tours, hors grâce de déplacement, met la ZONE en pause, comme une pause
//!   de l'utilisateur (`orchestrator.pause(zone, None)` : position et état
//!   enregistrés, aucune commande renvoyée à l'appareil). Une zone en pause
//!   n'entre plus dans la surveillance de lecture : un arrêt ultérieur de
//!   l'appareil ne relance rien. C'est le modèle de #6790 (OAAT).
//! - La reprise faite sur l'appareil (Play à la télécommande) est suivie :
//!   une zone mise en pause PAR L'APPAREIL repasse en lecture quand l'appareil
//!   rejoue NOTRE flux (`qui_tient_le_renderer` = `LeNotre`). Sans ce volet,
//!   la pause/lecture à la télécommande, qui marchait, laisserait la zone en
//!   pause pendant que la musique joue.
//!
//! Hors périmètre (sans go) : transformer une pause de l'appareil en ARRÊT.

use super::*;

/// Tours `Paused` d'affilée avant de croire à une pause faite sur l'appareil.
/// Même ordre que [`STOPPED_TICKS_THRESHOLD`] : un renderer qui passe un
/// instant par `PAUSED_PLAYBACK` pendant un chargement ou une reprise lente
/// (Devialet, fil 1780) n'emmène pas la zone.
pub(super) const TOURS_DE_PAUSE_DE_L_APPAREIL: u8 = 5;

/// Les sorties dont une pause peut venir de l'appareil lui-même et que ce
/// correctif couvre. Les sorties locales ne se mettent en pause que sur ordre
/// de Tune ; OAAT a son propre signal (#6790).
pub(super) fn sortie_concernee(output_type: &str) -> bool {
    output_type == "dlna"
}

/// Ce tour `Paused` achève-t-il une pause faite sur l'appareil ?
///
/// `tours` est le compte APRÈS ce tour. La grâce de déplacement écarte la
/// reprise de Tune elle-même (Seek de reprise, puis Play qui n'a pas encore
/// pris).
pub(super) fn pause_de_l_appareil_etablie(
    output_type: &str,
    tours: u8,
    en_grace_de_deplacement: bool,
) -> bool {
    sortie_concernee(output_type)
        && !en_grace_de_deplacement
        && tours >= TOURS_DE_PAUSE_DE_L_APPAREIL
}

impl PositionPoller {
    /// La zone suit la pause de son appareil : état `paused` en mémoire et en
    /// base, position enregistrée, aucune commande vers l'appareil.
    pub(super) async fn suivre_la_pause_de_l_appareil(&self, zone_id: i64, device_id: &str) {
        info!(
            zone_id,
            device = %device_id,
            "zone_en_pause_l_appareil_s_est_mis_en_pause"
        );
        if let Ok(mut zones) = self.pauses_de_l_appareil.lock() {
            zones.insert(zone_id);
        }
        let _ = self.orchestrator.pause(zone_id, None).await;
    }

    /// La zone joue de nouveau par Tune : la pause de l'appareil est close.
    pub(super) fn oublier_la_pause_de_l_appareil(&self, zone_id: i64) {
        if let Ok(mut zones) = self.pauses_de_l_appareil.lock() {
            zones.remove(&zone_id);
        }
    }

    /// Chemin « repos » : une zone mise en pause par son appareil repasse en
    /// lecture quand l'appareil rejoue NOTRE flux. Rend `true` si la zone a
    /// repris.
    pub(super) async fn suivre_la_reprise_de_l_appareil(
        &self,
        zone_id: i64,
        device_id: &str,
        status: &OutputStatus,
        etats: &[crate::playback::ZoneState],
    ) -> bool {
        if status.state != TransportState::Playing {
            return false;
        }
        let suivie = self
            .pauses_de_l_appareil
            .lock()
            .map(|z| z.contains(&zone_id))
            .unwrap_or(false);
        if !suivie {
            return false;
        }
        let Some(etat) = etats.iter().find(|s| s.zone_id == zone_id) else {
            return false;
        };
        if etat.state != PlayState::Paused {
            // Reprise ou arrêt par Tune : ce n'est plus la pause de l'appareil.
            self.oublier_la_pause_de_l_appareil(zone_id);
            return false;
        }
        let notre_flux = etat
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref());
        if decisions::qui_tient_le_renderer(status.current_uri.as_deref(), notre_flux)
            != decisions::TenueDuRenderer::LeNotre
        {
            // Une autre application joue sur l'appareil (Emby…) : la zone
            // reste en pause, et rien ne part.
            return false;
        }
        info!(
            zone_id,
            device = %device_id,
            position_ms = status.position_ms,
            "zone_reprend_l_appareil_a_repris_notre_flux"
        );
        self.oublier_la_pause_de_l_appareil(zone_id);
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "playing")
            .ok();
        self.playback.resume(zone_id).await;
        true
    }
}
