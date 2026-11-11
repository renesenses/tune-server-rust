//! #8402 (décisions de Bertrand, 11/11) — une zone AirPlay 2 en PURE
//! verrouillé ouvre sa session à 0 %, plus à 100 % : l'orchestrateur le dit
//! à l'utilisateur. Le volume fixe, lui, garde son départ à 100 % (l'ampli
//! tient le volume) : rien à dire.
//!
//! Canal : `zone.playback_error` NON fatal, comme le repli de périphérique
//! (#2269) et le déplacement refusé — la zone joue. Le code stable nomme le
//! cas ; le web le traduit (`erreursServeur.<code>`) et l'affiche même dans
//! la fenêtre de grâce qui suit un Lire.
//!
//! Une fois par session ouverte, jamais à chaque piste : la sortie ne lève
//! son drapeau qu'à l'ouverture (`prendre_le_depart_muet`).

use super::*;
use crate::outputs::airplay2::Airplay2Output;
use crate::outputs::airplay2::volume_de_depart_8402::pure_verrouille;
use crate::outputs::traits::OutputTarget;

/// Code : PURE verrouillé, la session AirPlay 2 est partie à 0 %.
pub const CODE_DEPART_A_ZERO_PURE: &str = "airplay2_start_volume_zero_pure";

/// La charge de `zone.playback_error` pour un départ à 0 %.
pub(super) fn charge_du_depart_muet(zone_id: i64) -> serde_json::Value {
    serde_json::json!({
        "zone_id": zone_id,
        "code": CODE_DEPART_A_ZERO_PURE,
        "error": "AirPlay 2: this zone's volume is locked by PURE, so playback started \
                  at 0 % to protect your ears and speakers. Turn the volume up on the \
                  device or in Tune.",
        // La zone JOUE : seul le niveau de départ est en cause.
        "fatal": false,
    })
}

impl PlaybackOrchestrator {
    /// Avant la piste : dit à une sortie AirPlay 2 si sa zone est en PURE
    /// verrouillé. Lu en base à chaque piste, réglage changé en marche compris.
    pub(super) fn dire_a_l_airplay2_si_la_zone_tient_le_volume(
        &self,
        zone_id: i64,
        sortie: &dyn OutputTarget,
    ) {
        if let Some(airplay2) = sortie.as_any().downcast_ref::<Airplay2Output>() {
            airplay2.tenir_le_volume_de_la_zone(pure_verrouille(&self.db, zone_id));
        }
    }

    /// Après une piste partie : la session s'est-elle ouverte à 0 % ? Le
    /// dire à toutes les télécommandes.
    pub(super) fn dire_le_depart_muet_d_airplay2(&self, zone_id: i64, sortie: &dyn OutputTarget) {
        let Some(airplay2) = sortie.as_any().downcast_ref::<Airplay2Output>() else {
            return;
        };
        if !airplay2.prendre_le_depart_muet() {
            return;
        }
        warn!(zone_id, "airplay2_depart_a_zero_annonce");
        if let Some(ref bus) = self.event_bus {
            bus.emit("zone.playback_error", charge_du_depart_muet(zone_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_charge_dit_le_cas_sans_arreter_la_zone_8402() {
        let pure = charge_du_depart_muet(7);
        assert_eq!(pure["zone_id"], 7);
        assert_eq!(pure["code"], CODE_DEPART_A_ZERO_PURE);
        assert_eq!(pure["fatal"], false);
        assert!(pure["error"].as_str().unwrap().contains("in Tune"));
    }
}
