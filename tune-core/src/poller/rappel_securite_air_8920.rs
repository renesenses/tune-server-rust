//! #8920 (décision de Bertrand du 20/11) — un ampli Devialet AIR monté à la
//! télécommande AU-DESSUS de la limite de sécurité de sa zone y est ramené
//! par le sondeur, comme les autres sorties (#6541, #7379).
//!
//! Le site générique (`volume_plafond_6541`) ne le voit pas : sur AIR,
//! `get_status` rend la consigne de Tune (`volume_voulu`), jamais le niveau
//! de l'ampli. Le niveau réel n'arrive que par l'annonce, lue ici
//! (`relever_controles_air`).
//!
//! - La cible est la limite de sécurité en dB d'ampli, ramenée vers le bas
//!   sur la grille de 0,5 dB (`securite_en_db`, la même que le départ de
//!   lecture `ramener_sous_la_securite`) : jamais au-dessus de la limite.
//! - Un rappel par dépassement : une fois par niveau annoncé. Un ampli qui
//!   ne suit pas n'est pas relancé à chaque tour ; l'épisode se referme
//!   quand l'ampli repasse à la limite ou dessous. Un nouveau geste au-dessus
//!   (autre niveau) est rappelé à son tour.
//! - La commande passe par le point de passage unique de l'orchestrateur :
//!   la consigne de la zone devient la limite, en mémoire et en base, et
//!   l'évènement `volume` la publie à l'interface. Elle part en origine
//!   « pas » et en hausse confirmée : c'est une BAISSE de l'ampli, que ni le
//!   confort ni la garde de hausse (repérés sur la consigne d'avant) ne
//!   doivent retenir plus bas que la limite.
//! - Aucun effet sous la limite, ni sans limite réglée. Volume fixe : refusé
//!   sur AIR, jamais concerné. PURE : la limite y reste en vigueur sur AIR
//!   (`mode_sans_limite`), le rappel aussi.
use super::*;
use crate::audio::garde_de_hausse::Hausse;
use crate::audio::volume_limites;
use crate::outputs::devialet_air::{reglages::BornesDb, sortie::securite_en_db};
use crate::playback::ObservationVolumeAppareil;

/// Tolérance de comparaison en dB : l'ampli parle sur une grille de 0,5 dB.
const TOLERANCE_DB: f64 = 0.01;

impl PositionPoller {
    /// `true` quand l'annonce `observation` dépasse la limite de sécurité de
    /// la zone et que ce dépassement n'a pas encore été rappelé.
    pub(super) fn depassement_air_a_rappeler(
        &self,
        zone_id: i64,
        observation: &ObservationVolumeAppareil,
    ) -> bool {
        let securite_db = volume_limites::lire_effectives(&self.db, zone_id)
            .securite
            .map(|s| securite_en_db(BornesDb::lues(&self.db), s));
        let Ok(mut vus) = self.rappels_air_securite.lock() else {
            return false;
        };
        if !securite_db.is_some_and(|s| observation.volume_db > s + TOLERANCE_DB) {
            vus.remove(&zone_id);
            return false;
        }
        let deja = vus
            .get(&zone_id)
            .is_some_and(|v| (v - observation.volume_db).abs() <= TOLERANCE_DB);
        vus.insert(zone_id, observation.volume_db);
        !deja
    }

    /// Ramène l'ampli de `zone_id` à la limite de sécurité, et le journalise.
    pub(super) async fn rappeler_air_a_la_securite(
        &self,
        zone_id: i64,
        device_id: &str,
        observation: &ObservationVolumeAppareil,
    ) {
        let Some(securite) = volume_limites::lire_effectives(&self.db, zone_id).securite else {
            return;
        };
        let course = BornesDb::lues(&self.db);
        let securite_db = securite_en_db(course, securite);
        let cible = course.db_vers_volume(securite_db);
        warn!(
            zone_id,
            rapporte_db = observation.volume_db,
            securite_db,
            cible,
            "devialet_air_ramene_a_la_securite_8920"
        );
        match self
            .orchestrator
            .set_volume_au_pas_avec_garde(zone_id, cible, Some(device_id), Hausse::Confirmee)
            .await
        {
            Ok(r) => info!(
                zone_id,
                securite_db,
                retenu = r.retenu,
                "devialet_air_ramene_a_la_securite_ok_8920"
            ),
            Err(e) => warn!(
                zone_id,
                securite_db,
                error = %e,
                "devialet_air_ramene_a_la_securite_echec_8920"
            ),
        }
    }
}
