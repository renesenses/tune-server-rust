//! #6264 — reprise par NOUVEAU flux après une longue pause.
//!
//! Décision de Bertrand du 13/10 : l'Eversolo DMP-A8 (`Lavf/58.45.100`)
//! précharge le morceau entier et relit mal sa mémoire quand il reprend après
//! une longue pause. Sur le .18 (zone 10, 13/10), un FLAC servi tel quel,
//! 27 min de pause, puis `Play` : aucune requête HTTP, et un bruit blanc fort
//! à 132 s ; le fichier, lui, est sain. Le défaut est dans le lecteur.
//!
//! Le contournement, propre aux zones qui l'arment : passé un seuil de pause
//! (5 min par défaut, réglable par zone), la reprise relance le morceau à la
//! position mémorisée — nouvelle URI, puis `Seek` — au lieu d'un `Play` sur
//! place. C'est le chemin « rétablir à la position » de `resume`, celui d'une
//! session morte. Les autres lecteurs ne changent pas.
//!
//! Le modèle est repéré par son identité (catalogue embarqué, quirk
//! `reprise_par_nouveau_flux`, résolu par `resolve_zone_quirks` : marque et
//! modèle choisis à la main, sinon ceux de la description UPnP détectée), et
//! se règle par zone : `zone_{id}_reprise_nouveau_flux` (`true` / `false`,
//! absent = ce que dit le modèle) et `zone_{id}_reprise_nouveau_flux_apres_s`
//! (secondes).

use std::sync::Arc;
use std::time::Duration;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Le seuil de pause par défaut : 5 min.
pub const SEUIL_PAR_DEFAUT: Duration = Duration::from_secs(300);

/// La clé de réglage qui arme la reprise par nouveau flux sur la zone.
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_reprise_nouveau_flux")
}

/// La clé du seuil de pause, en secondes.
pub fn cle_seuil(zone_id: i64) -> String {
    format!("zone_{zone_id}_reprise_nouveau_flux_apres_s")
}

/// Le seuil de pause de la zone, `None` quand la reprise par nouveau flux
/// n'y est pas armée : le réglage de zone d'abord, sinon le modèle. Un seuil
/// illisible retombe sur le défaut.
pub fn seuil_de_la_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<Duration> {
    let settings = SettingsRepo::with_backend(db.clone());
    let reglage = settings.get(&cle(zone_id)).ok().flatten();
    let arme = match reglage.as_deref().map(str::trim) {
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => crate::device_catalog::resolve_zone_quirks(db, zone_id).reprise_par_nouveau_flux,
    };
    if !arme {
        return None;
    }
    let seuil = settings
        .get(&cle_seuil(zone_id))
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(SEUIL_PAR_DEFAUT);
    Some(seuil)
}

/// La reprise doit-elle passer par un nouveau flux ? Seulement si la zone
/// l'arme ET que la pause, DATÉE, atteint le seuil. Une pause non datée
/// (état restauré) ne conclut rien : on reprend comme avant.
pub fn relancer_par_nouveau_flux(seuil: Option<Duration>, pause: Option<Duration>) -> bool {
    matches!((seuil, pause), (Some(s), Some(p)) if p >= s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seule_une_zone_armee_apres_une_longue_pause_relance() {
        let cinq = Some(SEUIL_PAR_DEFAUT);
        assert!(relancer_par_nouveau_flux(
            cinq,
            Some(Duration::from_secs(27 * 60))
        ));
        assert!(relancer_par_nouveau_flux(cinq, Some(SEUIL_PAR_DEFAUT)));
        assert!(!relancer_par_nouveau_flux(
            cinq,
            Some(Duration::from_secs(299))
        ));
        assert!(!relancer_par_nouveau_flux(cinq, None), "pause non datée");
        assert!(
            !relancer_par_nouveau_flux(None, Some(Duration::from_secs(3600))),
            "zone non armée : aucun changement"
        );
    }

    #[test]
    fn le_reglage_de_zone_arme_et_regle_le_seuil() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        assert_eq!(seuil_de_la_zone(&db, 10), None);
        let settings = SettingsRepo::with_backend(db.clone());
        settings.set(&cle(10), "true").unwrap();
        assert_eq!(seuil_de_la_zone(&db, 10), Some(SEUIL_PAR_DEFAUT));
        settings.set(&cle_seuil(10), "600").unwrap();
        assert_eq!(seuil_de_la_zone(&db, 10), Some(Duration::from_secs(600)));
        assert_eq!(seuil_de_la_zone(&db, 11), None, "par zone");
        settings.set(&cle(10), "false").unwrap();
        assert_eq!(seuil_de_la_zone(&db, 10), None, "désarmé à la main");
    }

    /// Le modèle suffit : une zone dont l'identité est un Eversolo DMP-A8 est
    /// armée sans réglage, et un « false » de zone la désarme.
    #[test]
    fn le_dmp_a8_est_arme_par_son_modele() {
        assert!(crate::device_catalog::quirks_for("Eversolo", "DMP-A8").reprise_par_nouveau_flux);
        assert!(!crate::device_catalog::quirks_for("Eversolo", "DMP-A6").reprise_par_nouveau_flux);
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(db.clone());
        settings.set("zone_10_brand", "Eversolo").unwrap();
        settings.set("zone_10_model", "DMP-A8").unwrap();
        assert_eq!(seuil_de_la_zone(&db, 10), Some(SEUIL_PAR_DEFAUT));
        settings.set(&cle(10), "false").unwrap();
        assert_eq!(seuil_de_la_zone(&db, 10), None);
    }
}
