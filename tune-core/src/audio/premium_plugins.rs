//! Host policy for the four extracted premium features. Configuration keys
//! remain unchanged. The migration marker distinguishes legacy installations
//! from an intentional uninstall; absent flags must never reinstall a plugin.
use crate::db::settings_repo::SettingsRepo;
pub const IDS: [&str; 4] = ["equalizer", "crossfeed", "converter", "declick"];
pub const MIGRATION: &str = "premium_audio_plugins_migration_v1";
pub fn contains(id: &str) -> bool {
    IDS.contains(&id)
}
pub fn enabled(settings: &SettingsRepo, id: &str) -> bool {
    if !contains(id) || tune_plugin_native::failure(id).is_some() {
        return false;
    }
    match settings.get(MIGRATION) {
        Err(_) => return false,
        Ok(marker) if marker.as_deref() != Some("complete") => return true,
        _ => {}
    }
    settings
        .get(&format!("plugin_{id}_installed"))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
        && settings
            .get(&format!("plugin_{id}_enabled"))
            .is_ok_and(|v| v.as_deref() != Some("false"))
}
/// Idempotent upgrade. Preserve explicit disabled/uninstalled flags and all
/// zone settings/presets. Write the marker last so an interrupted retry is safe.
pub fn migrate(settings: &SettingsRepo) -> Result<(), String> {
    migrate_for_account(settings, true)
}
/// Existing premium accounts retain access. Other accounts see the four
/// installable entries without loading premium processors automatically.
pub fn migrate_for_account(settings: &SettingsRepo, premium: bool) -> Result<(), String> {
    if settings.get(MIGRATION)?.as_deref() == Some("complete") {
        return Ok(());
    }
    for id in IDS {
        for suffix in ["installed", "enabled"] {
            let key = format!("plugin_{id}_{suffix}");
            if settings.get(&key)?.is_none() {
                settings.set(&key, if premium { "true" } else { "false" })?;
            }
        }
    }
    settings.set(MIGRATION, "complete")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> SettingsRepo {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(std::sync::Arc::new(db))
    }
    #[test]
    fn premium_sdk_all_sixteen_installation_combinations_and_idempotent_migration() {
        let s = settings();
        s.set("zone_1_eq_profile", "legacy-profile").unwrap();
        migrate(&s).unwrap();
        for mask in 0..16 {
            for (i, id) in IDS.iter().enumerate() {
                let installed = mask & (1 << i) != 0;
                s.set(
                    &format!("plugin_{id}_installed"),
                    if installed { "true" } else { "false" },
                )
                .unwrap();
                assert_eq!(
                    enabled(&s, id),
                    installed,
                    "plugin {id} in combination {mask}"
                );
            }
            migrate(&s).unwrap();
            for (i, id) in IDS.iter().enumerate() {
                assert_eq!(
                    enabled(&s, id),
                    mask & (1 << i) != 0,
                    "migration resurrected {id}"
                );
            }
        }
        assert_eq!(
            s.get("zone_1_eq_profile").unwrap().as_deref(),
            Some("legacy-profile")
        );
        s.set("plugin_equalizer_installed", "true").unwrap();
        s.set("plugin_equalizer_enabled", "false").unwrap();
        assert!(!enabled(&s, "equalizer"));
        s.delete("plugin_equalizer_installed").unwrap();
        migrate(&s).unwrap();
        assert!(!enabled(&s, "equalizer"), "deleted plugin was reinstalled");
    }
}
