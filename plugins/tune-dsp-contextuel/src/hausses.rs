//! Les hausses de niveau, et l'interface vers la borne de remontée de TESS.
//!
//! Règle de Bertrand (#9540, #9547, #9549) : toute hausse de niveau rejoint
//! le niveau d'avant en environ 0,5 s, sans jamais le dépasser ; si le volume
//! a monté entre-temps, le surplus reste gardé (« niveau réduit », « Rendre
//! le niveau »). La rampe de 20 ms ne vaut que pour les BAISSES.
//!
//! Cette caisse ne remonte RIEN lentement elle-même : elle ne fait que
//! DÉCLARER la hausse qu'une bascule va causer (`Traitement::hausse_si`).
//! L'étage TESS ([`crate::tess::EtageContextuel`]) la confie, AVANT de
//! basculer, à une [`PorteDesHausses`] ; au branchement, celle-ci sera la
//! borne de #9547 (`borne_de_remontee` / `retenir_la_hausse_de`), qui
//! abaisse aussitôt le gain de la zone de la hausse annoncée puis le rend à
//! son rythme.

use std::sync::Mutex;

/// Ce à quoi un étage contextuel confie une hausse avant de la causer.
pub trait PorteDesHausses: Send + Sync {
    /// Une bascule va faire monter le niveau d'au plus `hausse_db` dB (> 0)
    /// au bloc suivant. Hors temps réel ; ne doit pas bloquer.
    fn retenir_la_hausse_de(&self, hausse_db: f64);
}

/// Une porte qui garde la trace des hausses annoncées (tests, journaux).
#[derive(Debug, Default)]
pub struct PorteEnregistreuse {
    hausses: Mutex<Vec<f64>>,
}

impl PorteEnregistreuse {
    pub fn hausses(&self) -> Vec<f64> {
        self.hausses.lock().map(|h| h.clone()).unwrap_or_default()
    }
}

impl PorteDesHausses for PorteEnregistreuse {
    fn retenir_la_hausse_de(&self, hausse_db: f64) {
        if let Ok(mut h) = self.hausses.lock() {
            h.push(hausse_db);
        }
    }
}
