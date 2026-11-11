//! #7418 — une pause faite SUR L'APPAREIL, côté orchestrateur.
//!
//! Le sondeur constate la pause de l'appareil (`poller::pause_de_l_appareil_7418`)
//! et décide quand elle devient un arrêt ; ici vivent les deux gestes qui en
//! découlent :
//!
//! - **la pause suivie** : la zone passe en pause sans commande vers
//!   l'appareil, et elle est notée. Sa reprise depuis Tune ne renvoie pas un
//!   `Play` « sur place » à un appareil qui a pu changer d'entrée ou être pris
//!   par une autre application (le `Play` y restait sans effet, ou rejouait
//!   la musique de l'autre) : elle RÉTABLIT la piste à la position, par un
//!   nouveau flux, comme la reprise d'un transfert en pause (#5050) ;
//! - **l'arrêt complet** : la zone s'arrête, sa piste et sa position gardées
//!   et armées pour la reprise (`pending_resume_ms`, #2876). « Lecture » dans
//!   Tune la relance donc à l'endroit de la pause. Un `Stop` ne part vers
//!   l'appareil que s'il tient encore NOTRE flux : un appareil passé à une
//!   autre entrée ou à une autre application n'est pas coupé.

use super::PlaybackOrchestrator;
use tracing::info;

impl PlaybackOrchestrator {
    /// La zone suit la pause de son appareil : état `paused` en mémoire et en
    /// base, position enregistrée, aucune commande vers l'appareil.
    pub async fn suivre_la_pause_de_l_appareil(&self, zone_id: i64) {
        let _ = self.pause(zone_id, None).await;
        self.pauses_de_l_appareil
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(zone_id);
    }

    /// La pause de l'appareil est close sans passer par `resume` (l'appareil
    /// a lui-même rejoué notre flux).
    pub fn oublier_la_pause_de_l_appareil(&self, zone_id: i64) {
        self.pauses_de_l_appareil
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&zone_id);
    }

    /// Vrai (une seule fois) si la zone est en pause parce que son appareil
    /// s'y est mis : sa reprise doit rétablir la piste à la position.
    pub(super) fn prendre_la_pause_de_l_appareil(&self, zone_id: i64) -> bool {
        self.pauses_de_l_appareil
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&zone_id)
    }

    /// Arrêt complet après une pause faite sur l'appareil.
    ///
    /// `arreter_l_appareil` : l'appareil tient encore notre flux, en pause ;
    /// le `Stop` l'empêche de le reprendre de lui-même plus tard (retour sur
    /// l'entrée réseau, téléviseur éteint — fil 2531). Faux quand l'appareil
    /// est déjà ailleurs : rien ne lui est envoyé.
    pub async fn arreter_apres_la_pause_de_l_appareil(
        &self,
        zone_id: i64,
        device_id: &str,
        arreter_l_appareil: bool,
    ) {
        self.oublier_la_pause_de_l_appareil(zone_id);
        let avant = self.playback.get_state(zone_id).await;
        let position_ms = avant.position_ms.max(0);
        if arreter_l_appareil {
            self.stop(zone_id, Some(device_id)).await;
        } else {
            self.ceder_la_zone_a_un_tiers(zone_id).await;
        }
        // Piste et position gardées, ARMÉES pour « Lecture » : sans le
        // marqueur, la reprise d'une zone arrêtée repartirait du début.
        if let Some(mut np) = avant.now_playing {
            // La session de flux est fermée avec l'arrêt.
            np.stream_id = None;
            self.playback
                .restore_position(zone_id, position_ms, np)
                .await;
        }
        info!(
            zone_id,
            device = %device_id,
            position_ms,
            stop_envoye = arreter_l_appareil,
            "zone_arretee_apres_la_pause_de_l_appareil"
        );
    }
}
