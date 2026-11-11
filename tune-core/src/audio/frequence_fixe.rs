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

/// Le choix reste mémorisé sous PURE/strict, mais ne transforme jamais le flux.
pub fn active(db: &Arc<dyn DbBackend>, id: i64) -> Option<u32> {
    if super::audiophile::zone_enabled(db, id) || super::bitperfect_strict::zone_enabled(db, id) {
        None
    } else {
        lire(db, id)
    }
}

/// Une cadence fixe ne se rabat pas silencieusement vers une autre famille.
/// Le plafond matériel reste opposable : une configuration incompatible échoue.
pub fn cible(
    db: &Arc<dyn DbBackend>,
    id: i64,
    plafond: Option<u32>,
) -> Result<Option<u32>, String> {
    let rate = active(db, id);
    if let Some(r) = rate
        && plafond.is_some_and(|max| r > max)
    {
        return Err(format!(
            "Fixed output sample rate {r} Hz exceeds the output limit {} Hz",
            plafond.unwrap()
        ));
    }
    Ok(rate)
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
        assert!(cible(&db, 8428, Some(44_100)).is_err());
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
