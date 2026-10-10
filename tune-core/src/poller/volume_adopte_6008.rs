//! #6008 — le sondeur adopte le volume que remonte le renderer, et le
//! journal le dit.
//!
//! Les trois sites d'adoption de `tick.rs` recopient en base (et en mémoire)
//! le volume qu'un appareil rapporte de lui-même quand il a VRAIMENT bougé
//! (`decisions::should_adopt_device_volume`). C'était silencieux : un Cabasse
//! Abyss qui remonte environ 80 % (fil 2187) changeait le volume de la zone
//! sans qu'aucune ligne ne permette de savoir d'où il venait.
//!
//! Chaque adoption écrit désormais une ligne INFO `volume_adopte_du_renderer`
//! avec `zone_id`, `appareil` (identifiant de la sortie), `ancien` et
//! `nouveau` (en %, arrondis au dixième).
//!
//! Débit : une ligne par changement réel, rien si la valeur ne change pas.
//! L'adoption est déjà déclenchée sur FRONT (l'appareil doit avoir bougé de
//! plus de 2 points depuis le tour précédent, et différer de la zone) : un
//! appareil qui répète sa valeur n'adopte rien, donc n'écrit rien. Reste le
//! cas où l'arrondi au dixième ne bouge pas (`ancien == nouveau`) : tu.
use super::*;

/// Un volume (fraction 0–1) en %, arrondi au dixième pour le journal.
fn en_pour_cent(volume: f64) -> f64 {
    (volume * 1000.0).round() / 10.0
}

/// Écrit la ligne d'adoption de `zone_id` si la valeur change vraiment.
/// Volumes en fraction 0–1 ; rend `true` quand la ligne est partie.
pub(super) fn journaliser_volume_adopte(
    zone_id: i64,
    appareil: &str,
    ancien: f64,
    nouveau: f64,
) -> bool {
    let (ancien, nouveau) = (en_pour_cent(ancien), en_pour_cent(nouveau));
    if ancien == nouveau {
        return false;
    }
    info!(
        zone_id,
        appareil, ancien, nouveau, "volume_adopte_du_renderer"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rien_si_l_arrondi_ne_bouge_pas() {
        assert!(journaliser_volume_adopte(7, "dlna-x", 1.0, 0.8));
        assert!(!journaliser_volume_adopte(7, "dlna-x", 0.8, 0.80001));
    }

    #[test]
    fn le_pour_cent_est_arrondi_au_dixieme() {
        assert_eq!(en_pour_cent(0.83), 83.0);
        assert_eq!(en_pour_cent(0.8004), 80.0);
        assert_eq!(en_pour_cent(0.0049), 0.5);
    }
}
