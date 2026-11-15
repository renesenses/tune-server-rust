//! #8137 (fil 1771) — la bascule PURE n'envoie une commande de volume à
//! l'appareil que si elle change quelque chose.
//!
//! Un renderer DLNA en volume fixe coupait son signal à chaque bascule PURE,
//! flux pourtant conservé. Sur une zone en volume fixe, l'entrée et la sortie
//! n'envoient plus rien (c'était déjà le cas dans `appliquer_volume`, mais
//! rien ne le disait au journal, et la sortie écrivait « recompose » comme si
//! une commande était partie). Sur une zone à volume variable, la commande
//! n'est pas envoyée non plus quand le dernier volume RELU sur l'appareil par
//! le sondeur est déjà la valeur visée. Chaque commande qui part pendant une
//! bascule est journalisée avec sa valeur, l'appareil et la durée de la
//! réponse.

use std::collections::HashMap;
use std::sync::Mutex;

/// Le geste de bascule qui commande le volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasculePure {
    /// PURE verrouillé s'arme : 100 % sans trim.
    Entree,
    /// PURE verrouillé se relâche : la consigne de la zone, trim compris (#6213).
    Sortie,
}

impl BasculePure {
    pub(crate) fn code(self) -> &'static str {
        match self {
            BasculePure::Entree => "entree",
            BasculePure::Sortie => "sortie",
        }
    }
}

/// Ce que la bascule a fait du volume de l'appareil.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnvoiDeVolume {
    /// Une commande est partie vers l'appareil, avec cette valeur.
    Envoye(f64),
    /// Zone en volume fixe, ou sortie sans volume réglable tenue à plein
    /// niveau (#5575) : aucune commande.
    AucunVolumeFixe,
    /// L'appareil a été relu à cette valeur, qui est celle visée : aucune
    /// commande.
    AucunDejaALaCible(f64),
    /// La zone n'a pas d'appareil : la consigne n'est que mémorisée.
    AucunSansAppareil,
    /// Une demande plus récente a remplacé celle-ci dans la file de la sortie.
    Remplace,
}

impl EnvoiDeVolume {
    /// Une commande est-elle partie vers l'appareil ?
    pub fn envoye(self) -> bool {
        matches!(self, EnvoiDeVolume::Envoye(_))
    }
}

/// Écart toléré entre le volume relu et la valeur visée : un demi-point, les
/// renderers rendant leur volume en points entiers (DLNA 0–100).
pub(crate) const TOLERANCE_DE_RELECTURE: f64 = 0.005;

/// Le dernier volume RELU sur chaque appareil par le sondeur, en échelle de
/// l'appareil (trim compris). Effacé dès qu'une commande de volume part vers
/// l'appareil : la relecture d'avant ne dit plus rien de ce qu'il joue.
#[derive(Debug, Default)]
pub(crate) struct VolumesRelus(Mutex<HashMap<i64, f64>>);

impl VolumesRelus {
    pub(crate) fn noter(&self, zone_id: i64, relu: f64) {
        if !relu.is_finite() {
            return;
        }
        if let Ok(mut m) = self.0.lock() {
            m.insert(zone_id, relu.clamp(0.0, 1.0));
        }
    }

    pub(crate) fn oublier(&self, zone_id: i64) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(&zone_id);
        }
    }

    pub(crate) fn connu(&self, zone_id: i64) -> Option<f64> {
        self.0.lock().ok().and_then(|m| m.get(&zone_id).copied())
    }
}

/// `Some(relu)` quand le volume relu est déjà la valeur visée.
pub(crate) fn deja_a_la_cible(relu: Option<f64>, cible: f64) -> Option<f64> {
    relu.filter(|r| cible.is_finite() && (r - cible).abs() <= TOLERANCE_DE_RELECTURE)
}

impl super::PlaybackOrchestrator {
    /// #8137 — le sondeur a relu ce volume sur l'appareil de la zone (échelle
    /// de l'appareil). À n'appeler que pour une relecture fiable : hors
    /// sourdine, hors délai de grâce après une commande, relevé lisible.
    pub fn noter_le_volume_relu(&self, zone_id: i64, relu: f64) {
        self.volumes_relus.noter(zone_id, relu);
    }

    /// #8137 / #6213 — la sortie du verrou PURE : la consigne de la zone,
    /// trim compris, comme [`Self::set_volume`], mais sans commande quand la
    /// zone est en volume fixe ou que l'appareil a été relu à cette valeur.
    /// À appeler APRÈS l'écriture du réglage, pour que le verrou ne morde plus.
    pub async fn set_volume_sortie_de_pure(
        &self,
        zone_id: i64,
        volume: f64,
        device_id: Option<&str>,
    ) -> super::OutputCommandResult<EnvoiDeVolume> {
        let pure_force = crate::audio::audiophile::volume_lock_enabled(&self.db, zone_id)
            && crate::audio::audiophile::zone_enabled(&self.db, zone_id);
        super::refuser_une_baisse_sous_pure_verrouille(zone_id, volume, pure_force)?;
        self.appliquer_volume(
            zone_id,
            volume,
            device_id,
            pure_force,
            crate::audio::volume_limites::OrigineDuVolume::Absolue,
            crate::audio::garde_de_hausse::Hausse::Gardee,
            Some(BasculePure::Sortie),
        )
        .await
        .map(|(_, envoi)| envoi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_cible_relue_est_reconnue_au_demi_point_pres_8137() {
        assert_eq!(deja_a_la_cible(Some(1.0), 1.0), Some(1.0));
        assert_eq!(deja_a_la_cible(Some(0.996), 1.0), Some(0.996));
        assert_eq!(deja_a_la_cible(Some(0.99), 1.0), None);
        assert_eq!(deja_a_la_cible(None, 1.0), None);
        assert_eq!(deja_a_la_cible(Some(0.5), f64::NAN), None);
    }

    #[test]
    fn une_commande_efface_la_relecture_8137() {
        let relus = VolumesRelus::default();
        relus.noter(3, 1.0);
        assert_eq!(relus.connu(3), Some(1.0));
        relus.oublier(3);
        assert_eq!(relus.connu(3), None);
        relus.noter(3, f64::NAN);
        assert_eq!(relus.connu(3), None);
    }
}
