//! #6739 — un relevé du volume de l'appareil ne remonte jamais le volume
//! utilisateur (décision de Bertrand du 22/10).
//!
//! Fil 2366/2375 (Marantz NR1200 en DLNA, tweeter détruit) : le trim de
//! +8,5 dB faisait partir `SetVolume 96` pour une consigne de 36 %, et les
//! trois sites d'adoption de `tick.rs` recopiaient ensuite le 96 relu comme
//! consigne. Le geste suivant le remultipliait : cliquet vers 100 %.
//!
//! Désormais, les trois sites passent par [`PositionPoller::releve_adoptable`]
//! : le relevé est ramené à l'échelle de la consigne (trim retiré), adopté
//! seulement s'il est plus BAS (un geste sur la télécommande de l'appareil),
//! et une ligne `volume_releve_plus_haut_non_adopte` dit le refus quand il
//! est plus haut. Débit : la règle est sur front (l'appareil doit avoir bougé
//! depuis le tour précédent), donc une ligne par mouvement, pas par tour.
use super::decisions::{JugementDuReleve, juger_le_releve_de_volume};
use super::*;

impl PositionPoller {
    /// La consigne à adopter d'après le volume `rapporte` par l'appareil, ou
    /// `None`. Jamais plus haute que `consigne`.
    pub(super) fn releve_adoptable(
        &self,
        zone_id: i64,
        appareil: &str,
        precedent: Option<f64>,
        rapporte: f64,
        consigne: f64,
    ) -> Option<f64> {
        // #6912 — AIR est suivi par son annonce fraîche, même arrêté.
        // Le repli get_status sur volume_voulu n'est pas une observation.
        if appareil.starts_with("devialet-air:") {
            return None;
        }
        // Le trim ne se lit en base que quand l'appareil a bougé.
        if !precedent.is_some_and(|p| (rapporte - p).abs() > 1e-9) {
            return None;
        }
        let trim_db = crate::orchestrator::gain_trim_db_enregistre(&self.db, zone_id);
        match juger_le_releve_de_volume(precedent, rapporte, consigne, trim_db) {
            JugementDuReleve::Adopter(v) => Some(v),
            JugementDuReleve::PlusHautNonAdopte(releve) => {
                info!(
                    zone_id,
                    appareil,
                    consigne,
                    releve,
                    rapporte,
                    trim_db,
                    "volume_releve_plus_haut_non_adopte"
                );
                None
            }
            JugementDuReleve::Ignorer => None,
        }
    }
}
