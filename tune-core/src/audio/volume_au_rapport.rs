//! #7841 — ce que le rapport de diagnostic dit du volume d'une zone.
//!
//! Linn Klimax en OpenHome : « à 30, plus de son du tout ». Entre le curseur
//! et l'ordre `SetVolume`, Tune compose le trim de gain de la zone
//! (`zone_{id}_gain_trim_db`, [`volume_appareil_avec_trim`]) puis borne par
//! la limite de sécurité ([`LimitesDeVolume::borner_appareil`]). Le rapport
//! n'en montrait rien : un trim de −12 dB, qui fait partir 30 % en 8 %, ne se
//! distinguait pas d'une échelle d'appareil trop sourde. Ces lignes suffisent
//! à trancher sur un rapport.
//!
//! Aucune donnée personnelle : des réglages de zone, en dB et en fractions.

use std::sync::Arc;

use serde::Serialize;

use crate::audio::volume_limites::{self, LimitesDeVolume, ModeSansLimite};
use crate::db::backend::DbBackend;
use crate::orchestrator::{
    gain_trim_db_enregistre, sortie_a_volume_logiciel, volume_appareil_avec_trim,
};

/// Les réglages qui agissent entre le curseur d'une zone et son appareil.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VolumeDeZoneAuRapport {
    /// Le volume de la zone, 0..1.
    pub volume: f64,
    /// Le trim enregistré, en dB (0 sans réglage).
    pub gain_trim_db: f64,
    /// Le trim positif est ignoré sur une sortie à volume matériel (#6739).
    pub gain_trim_ignore: bool,
    /// La limite de sécurité qui agit, 0..1 ; `None` sans limite.
    pub limite_securite: Option<f64>,
    /// La limite de confort qui agit, 0..1 ; `None` sans limite.
    pub limite_confort: Option<f64>,
    /// Le mode qui suspend les limites (`limite_refusee_volume_fixe`,
    /// `limite_refusee_zone_pure`), s'il y en a un.
    pub limites_suspendues: Option<&'static str>,
    /// La consigne qui part vers l'appareil pour ce volume, trim et sécurité
    /// compris, 0..1 — celle du journal `device_set_volume_sending`.
    pub consigne_appareil: f64,
}

/// Lit, pour une zone, ce qui compose sa consigne d'appareil.
pub fn lire(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    volume: f64,
    output_type: &str,
) -> VolumeDeZoneAuRapport {
    let gain_trim_db = gain_trim_db_enregistre(db, zone_id);
    let limites = volume_limites::lire_effectives(db, zone_id);
    let suspension = volume_limites::mode_sans_limite(db, zone_id);
    composer(volume, gain_trim_db, limites, suspension, output_type)
}

fn composer(
    volume: f64,
    gain_trim_db: f64,
    limites: LimitesDeVolume,
    suspension: Option<ModeSansLimite>,
    output_type: &str,
) -> VolumeDeZoneAuRapport {
    let volume_materiel = !sortie_a_volume_logiciel(output_type);
    VolumeDeZoneAuRapport {
        volume,
        gain_trim_db,
        gain_trim_ignore: volume_materiel && gain_trim_db > 0.0,
        limite_securite: limites.securite,
        limite_confort: limites.confort,
        limites_suspendues: suspension.map(ModeSansLimite::code),
        consigne_appareil: limites.borner_appareil(volume_appareil_avec_trim(
            volume,
            gain_trim_db,
            volume_materiel,
        )),
    }
}

fn pourcent(v: Option<f64>) -> String {
    v.map(|v| format!("{:.0} %", v * 100.0))
        .unwrap_or_else(|| "none".into())
}

impl VolumeDeZoneAuRapport {
    /// La ligne du rapport Markdown, sous la zone.
    pub fn ligne_markdown(&self) -> String {
        let mut ligne = format!(
            "volume {:.0} %, gain trim {:+.1} dB{}, safety limit {}, comfort limit {}",
            self.volume * 100.0,
            self.gain_trim_db,
            if self.gain_trim_ignore {
                " (ignored: hardware volume)"
            } else {
                ""
            },
            pourcent(self.limite_securite),
            pourcent(self.limite_confort),
        );
        if let Some(code) = self.limites_suspendues {
            ligne.push_str(&format!(" (limits suspended: {code})"));
        }
        ligne.push_str(&format!(
            " → device setpoint {:.1} %",
            self.consigne_appareil * 100.0
        ));
        ligne
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_trim_negatif_et_la_securite_se_lisent_dans_la_ligne() {
        let r = composer(
            0.3,
            -12.0,
            LimitesDeVolume {
                securite: Some(0.25),
                confort: None,
            },
            None,
            "openhome",
        );
        // 0,3 × 10^(−12/20) ≈ 0,0754, sous la sécurité de 25 %.
        assert!((r.consigne_appareil - 0.0754).abs() < 1e-3, "{r:?}");
        let ligne = r.ligne_markdown();
        assert!(ligne.contains("gain trim -12.0 dB"), "{ligne}");
        assert!(ligne.contains("safety limit 25 %"), "{ligne}");
        assert!(ligne.contains("device setpoint 7.5 %"), "{ligne}");
    }

    #[test]
    fn un_trim_positif_est_dit_ignore_sur_un_volume_materiel() {
        let r = composer(0.3, 6.0, LimitesDeVolume::default(), None, "openhome");
        assert!(r.gain_trim_ignore);
        assert!((r.consigne_appareil - 0.3).abs() < 1e-9);
        assert!(r.ligne_markdown().contains("(ignored: hardware volume)"));
    }
}
