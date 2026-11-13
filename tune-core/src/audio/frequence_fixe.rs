//! #8428 — cadence PCM fixe par zone, indépendante de la famille source.
use crate::db::{backend::DbBackend, settings_repo::SettingsRepo};
use std::sync::Arc;

pub const CADENCES: &[u32] = &[
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

pub fn cle_de_zone(id: i64) -> String {
    format!("zone_{id}_fixed_sample_rate")
}

pub fn lire(db: &Arc<dyn DbBackend>, id: i64) -> Option<u32> {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(id))
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .filter(|r| CADENCES.contains(r))
}

/// AirPlay impose sa cadence de transport ; ce réglage PCM ne la remplace pas.
pub fn sortie_compatible(type_sortie: &str) -> bool {
    !matches!(type_sortie, "airplay" | "airplay2")
}

/// Le choix reste mémorisé sous PURE/strict, sans transformer le flux.
pub fn demandee_active(db: &Arc<dyn DbBackend>, id: i64) -> Option<u32> {
    if super::audiophile::zone_enabled(db, id) || super::bitperfect_strict::zone_enabled(db, id) {
        return None;
    }
    let zone = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(id)
        .ok()
        .flatten();
    if zone
        .as_ref()
        .and_then(|z| z.output_type.as_deref())
        .is_some_and(|t| !sortie_compatible(t))
    {
        return None;
    }
    lire(db, id)
}

/// Cadence effective : choix demandé, borné par la zone et le catalogue.
/// Décision de Bertrand du 12/11 : ramener au plafond au lieu d'échouer.
pub fn active(db: &Arc<dyn DbBackend>, id: i64) -> Option<u32> {
    let demandee = demandee_active(db, id)?;
    let zone = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(id)
        .ok()
        .flatten();
    let plafond = crate::device_catalog::combine_max_sample_rate(
        zone.and_then(|z| z.max_sample_rate),
        crate::device_catalog::resolve_zone_quirks(db, id).max_sample_rate,
    );
    Some(
        plafond
            .filter(|p| *p > 0)
            .map_or(demandee, |p| demandee.min(p)),
    )
}

/// Le chemin de lecture peut connaître un plafond plus précis que la zone.
pub fn cible(
    db: &Arc<dyn DbBackend>,
    id: i64,
    plafond: Option<u32>,
) -> Result<Option<u32>, String> {
    Ok(active(db, id).map(|r| plafond.filter(|p| *p > 0).map_or(r, |p| r.min(p))))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frequence_fixe_8428_persiste_et_respecte_pure_et_strict() {
        let sqlite = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        sqlite.init_schema().unwrap();
        crate::db::migrations::run_migrations(&sqlite).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(sqlite);
        let s = SettingsRepo::with_backend(db.clone());
        assert_eq!(active(&db, 8428), None);
        s.set(&cle_de_zone(8428), "48000").unwrap();
        assert_eq!(
            cible(&db, 8428, None).unwrap(),
            Some(48_000),
            "la cadence fixe doit conserver 48 kHz hors famille"
        );
        assert_eq!(cible(&db, 8428, Some(44_100)).unwrap(), Some(44_100));
        s.set(&super::super::bitperfect_strict::cle_de_zone(8428), "true")
            .unwrap();
        assert_eq!(active(&db, 8428), None);
        assert_eq!(lire(&db, 8428), Some(48_000));
        s.delete(&super::super::bitperfect_strict::cle_de_zone(8428))
            .unwrap();
        s.set("zone_8428_audiophile", r#"{"enabled":true}"#)
            .unwrap();
        assert_eq!(active(&db, 8428), None);
    }
}
