//! #8513 — la mesure de pièce entend la pièce NUE.
//!
//! Pendant le balayage d'une mesure au micro (greffon `calibration`), la zone
//! joue sans aucun de ses traitements : égaliseur, correction de pièce (FIR),
//! crossfeed, crosstalk, repli mono, ReplayGain, suréchantillonnage… Le moyen
//! est celui de PURE, déjà éprouvé : `Orchestrator::zone_audiophile` répond
//! « contourné » tant qu'une mesure tient la zone. Les RÉGLAGES ne sont pas
//! touchés (rien n'est écrit en base), le volume et son verrou non plus : la
//! mesure règle elle-même le volume et le rend.
//!
//! Sans cela, une mesure faite égaliseur actif se mesurait elle-même, et la
//! proposition qui en sortait REMPLAÇAIT l'égaliseur au lieu de s'y ajouter :
//! deux mesures de suite rendaient des corrections « pratiquement identiques »
//! (Xavier Joly, 11/11).
//!
//! Le rétablissement est garanti par `Drop` : fin normale, refus, annulation,
//! erreur, et même panique du fil de mesure (déroulement de la pile).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

static ZONES: LazyLock<Mutex<HashMap<i64, usize>>> = LazyLock::new(Default::default);

fn zones() -> MutexGuard<'static, HashMap<i64, usize>> {
    // Une panique ailleurs ne doit pas bloquer le rétablissement.
    ZONES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tant qu'il vit, la zone joue sans ses traitements.
#[derive(Debug)]
#[must_use = "le contournement cesse dès que la garde est rendue"]
pub struct Contournement {
    zone_id: i64,
}

impl Contournement {
    /// Contourne les traitements de `zone_id` jusqu'à la fin de la garde.
    /// Deux gardes sur la même zone se cumulent : la dernière rendue rétablit.
    pub fn poser(zone_id: i64) -> Self {
        *zones().entry(zone_id).or_insert(0) += 1;
        tracing::info!(zone_id, "mesure_nue_traitements_contournes");
        Self { zone_id }
    }

    pub fn zone_id(&self) -> i64 {
        self.zone_id
    }
}

impl Drop for Contournement {
    fn drop(&mut self) {
        let mut z = zones();
        if let Some(n) = z.get_mut(&self.zone_id) {
            *n -= 1;
            if *n == 0 {
                z.remove(&self.zone_id);
            }
        }
        tracing::info!(zone_id = self.zone_id, "mesure_nue_traitements_retablis");
    }
}

/// Une mesure tient-elle la zone en ce moment ?
pub fn active(zone_id: i64) -> bool {
    zones().contains_key(&zone_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_garde_contourne_puis_retablit() {
        assert!(!active(851_301));
        let g = Contournement::poser(851_301);
        assert!(active(851_301));
        assert!(!active(851_302), "une autre zone n'est pas touchée");
        drop(g);
        assert!(!active(851_301));
    }

    #[test]
    fn deux_gardes_se_cumulent() {
        let a = Contournement::poser(851_303);
        let b = Contournement::poser(851_303);
        drop(a);
        assert!(active(851_303), "la seconde garde tient encore la zone");
        drop(b);
        assert!(!active(851_303));
    }

    #[test]
    fn une_panique_pendant_la_mesure_retablit_quand_meme() {
        let r = std::panic::catch_unwind(|| {
            let _g = Contournement::poser(851_304);
            assert!(active(851_304));
            panic!("la mesure plante");
        });
        assert!(r.is_err());
        assert!(
            !active(851_304),
            "traitements restés contournés après une panique"
        );
    }
}
