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
//! ## L'arrêt complet (rc12, go de Bertrand, fil 2531)
//!
//! La pause suivie ne suffisait pas. r5yk5 demande, comme le fait Symfonium,
//! qu'une pause venue de l'appareil soit un ARRÊT : rien ne doit repartir
//! plus tard, et la lecture doit repartir normalement depuis Tune. Deux
//! défauts restaient :
//!
//! - l'appareil garde NOTRE flux en pause : quand le WiiM revient sur son
//!   entrée réseau (téléviseur éteint), il peut le reprendre de lui-même, et
//!   la zone suivait cette reprise ;
//! - l'appareil passé à autre chose (autre entrée, Emby, arrêt) laissait la
//!   zone « en pause » : « Lecture » dans Tune envoyait un `Play` sur place à
//!   un appareil qui n'avait plus notre piste — rien, ou la musique de
//!   l'autre application.
//!
//! Une zone mise en pause par son appareil est donc ARRÊTÉE ([`Suite`]) :
//!
//! - dès que l'appareil est ailleurs, après [`decisions::RELEVES_AVANT_DE_CEDER`]
//!   relevés concordants (arrêté, sans média, autre URI) — sans aucune
//!   commande : on ne coupe ni l'autre entrée ni l'autre application ;
//! - quand il reste en pause sur notre flux plus de
//!   [`DELAI_AVANT_L_ARRET_DE_L_APPAREIL`] — avec un `Stop`, pour qu'il ne
//!   puisse plus le reprendre seul.
//!
//! Dans les deux cas la piste et la position restent armées : « Lecture »
//! dans Tune relance un nouveau flux à l'endroit de la pause. Avant ce délai,
//! la lecture à la télécommande reprend la zone comme en rc11, et « Lecture »
//! dans Tune rétablit aussi un nouveau flux (orchestrateur).

use super::*;

/// Tours `Paused` d'affilée avant de croire à une pause faite sur l'appareil.
/// Même ordre que [`STOPPED_TICKS_THRESHOLD`] : un renderer qui passe un
/// instant par `PAUSED_PLAYBACK` pendant un chargement ou une reprise lente
/// (Devialet, fil 1780) n'emmène pas la zone.
pub(super) const TOURS_DE_PAUSE_DE_L_APPAREIL: u8 = 5;

/// Pause tenue sur notre flux au-delà de laquelle elle devient un arrêt
/// complet, `Stop` compris. Assez long pour qu'une pause à la télécommande
/// (téléphone, porte) se reprenne à la télécommande ; plus court que les
/// « 10 à 15 minutes » après lesquelles le WiiM de r5yk5 repartait seul.
pub(super) const DELAI_AVANT_L_ARRET_DE_L_APPAREIL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

/// Ce que le sondeur retient d'une zone mise en pause par son appareil.
#[derive(Debug, Clone, Copy)]
pub(super) struct SuiviDeLaPause {
    /// Instant où la zone a suivi la pause de l'appareil.
    pub(super) depuis: std::time::Instant,
    /// Relevés d'affilée où l'appareil n'était plus en pause sur notre flux.
    pub(super) releves_ailleurs: u8,
}

/// Ce que devient une zone mise en pause par son appareil, à ce relevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Suite {
    /// Rien à faire : l'appareil est toujours en pause sur notre flux, ou
    /// l'écart n'est pas encore confirmé.
    Attendre,
    /// L'appareil rejoue notre flux (télécommande) : la zone reprend.
    Reprendre,
    /// L'appareil est passé à autre chose : arrêt complet, rien ne lui est
    /// envoyé.
    ArreterSansCommande,
    /// L'appareil garde notre flux en pause depuis trop longtemps : arrêt
    /// complet, `Stop` compris.
    ArreterLAppareil,
}

/// Où en est l'appareil par rapport à notre flux, à ce relevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Releve {
    /// Il joue notre flux.
    JoueLeNotre,
    /// Il est en pause (ou en transition) sur notre flux.
    EnPauseSurLeNotre,
    /// Arrêté, sans média, ou sur une autre URI.
    Ailleurs,
}

pub(super) fn releve(state: TransportState, le_notre: bool) -> Releve {
    match state {
        TransportState::Playing if le_notre => Releve::JoueLeNotre,
        TransportState::Paused | TransportState::Transitioning if le_notre => {
            Releve::EnPauseSurLeNotre
        }
        _ => Releve::Ailleurs,
    }
}

/// La décision, une fois `releves_ailleurs` mis à jour par ce relevé.
pub(super) fn suite_de_la_pause(
    releve: Releve,
    releves_ailleurs: u8,
    duree_de_la_pause: std::time::Duration,
) -> Suite {
    match releve {
        Releve::JoueLeNotre => Suite::Reprendre,
        Releve::Ailleurs if releves_ailleurs >= decisions::RELEVES_AVANT_DE_CEDER => {
            Suite::ArreterSansCommande
        }
        Releve::Ailleurs => Suite::Attendre,
        Releve::EnPauseSurLeNotre if duree_de_la_pause >= DELAI_AVANT_L_ARRET_DE_L_APPAREIL => {
            Suite::ArreterLAppareil
        }
        Releve::EnPauseSurLeNotre => Suite::Attendre,
    }
}

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
            zones.insert(
                zone_id,
                SuiviDeLaPause {
                    depuis: std::time::Instant::now(),
                    releves_ailleurs: 0,
                },
            );
        }
        self.orchestrator
            .suivre_la_pause_de_l_appareil(zone_id)
            .await;
    }

    /// La zone joue de nouveau par Tune : la pause de l'appareil est close.
    pub(super) fn oublier_la_pause_de_l_appareil(&self, zone_id: i64) {
        if let Ok(mut zones) = self.pauses_de_l_appareil.lock() {
            zones.remove(&zone_id);
        }
    }

    /// Chemin « repos » : une zone mise en pause par son appareil reprend
    /// avec lui s'il rejoue NOTRE flux, ou s'arrête complètement s'il est
    /// passé à autre chose ou reste en pause trop longtemps. Rend `true` si
    /// la zone a changé d'état (le tour de cette zone est fini).
    pub(super) async fn suivre_l_appareil_apres_sa_pause(
        &self,
        zone_id: i64,
        device_id: &str,
        status: &OutputStatus,
        etats: &[crate::playback::ZoneState],
    ) -> bool {
        let suivi = self
            .pauses_de_l_appareil
            .lock()
            .ok()
            .and_then(|z| z.get(&zone_id).copied());
        let Some(suivi) = suivi else {
            return false;
        };
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
        let le_notre = decisions::qui_tient_le_renderer(status.current_uri.as_deref(), notre_flux)
            == decisions::TenueDuRenderer::LeNotre;
        let releve = releve(status.state, le_notre);
        let releves_ailleurs = if releve == Releve::Ailleurs {
            suivi.releves_ailleurs.saturating_add(1)
        } else {
            0
        };
        if let Ok(mut zones) = self.pauses_de_l_appareil.lock()
            && let Some(s) = zones.get_mut(&zone_id)
        {
            s.releves_ailleurs = releves_ailleurs;
        }
        match suite_de_la_pause(releve, releves_ailleurs, suivi.depuis.elapsed()) {
            Suite::Attendre => false,
            Suite::Reprendre => {
                info!(
                    zone_id,
                    device = %device_id,
                    position_ms = status.position_ms,
                    "zone_reprend_l_appareil_a_repris_notre_flux"
                );
                self.oublier_la_pause_de_l_appareil(zone_id);
                self.orchestrator.oublier_la_pause_de_l_appareil(zone_id);
                crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                    .save_play_state(zone_id, "playing")
                    .ok();
                self.playback.resume(zone_id).await;
                true
            }
            suite @ (Suite::ArreterSansCommande | Suite::ArreterLAppareil) => {
                self.oublier_la_pause_de_l_appareil(zone_id);
                self.orchestrator
                    .arreter_apres_la_pause_de_l_appareil(
                        zone_id,
                        device_id,
                        suite == Suite::ArreterLAppareil,
                    )
                    .await;
                true
            }
        }
    }
}
