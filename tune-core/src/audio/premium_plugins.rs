//! Host policy for the four extracted premium features. Configuration keys
//! remain unchanged. The migration marker distinguishes legacy installations
//! from an intentional uninstall; absent flags must never reinstall a plugin.
//!
//! Depuis la v0.9.156, l'égaliseur est un greffon FACULTATIF, installé depuis
//! le catalogue : la migration ne pose plus `plugin_equalizer_installed` ni
//! `plugin_equalizer_enabled`. Une configuration existante (`zone_*_eq_profile`
//! ou `eq_presets`) ne déclenche qu'une PROPOSITION d'installation
//! (`plugin_equalizer_install_proposed=true`) ; aucun réglage n'est touché.
//! Installer (la route `POST /plugins/equalizer/install` pose
//! `plugin_equalizer_installed=true`) rend les réglages actifs tels quels.
//! Crossfeed, convertisseur et Dé-ploc gardent la migration d'origine.
use crate::db::settings_repo::SettingsRepo;
pub const IDS: [&str; 4] = ["equalizer", "crossfeed", "converter", "declick"];
pub const MIGRATION: &str = "premium_audio_plugins_migration_v1";
pub fn contains(id: &str) -> bool {
    IDS.contains(&id)
}
pub fn requires_premium(id: &str) -> bool {
    contains(id) && id != "equalizer"
}
/// Les greffons que la migration embarque-active elle-même. L'égaliseur n'en
/// fait plus partie : il s'installe depuis le catalogue.
fn installed_by_migration(id: &str) -> bool {
    contains(id) && id != "equalizer"
}
fn flag(settings: &SettingsRepo, id: &str, suffix: &str) -> Option<String> {
    settings
        .get(&format!("plugin_{id}_{suffix}"))
        .ok()
        .flatten()
}
/// `plugin_{id}_installed == "true"`, sans fenêtre legacy.
pub fn installed(settings: &SettingsRepo, id: &str) -> bool {
    contains(id) && flag(settings, id, "installed").as_deref() == Some("true")
}
pub fn enabled(settings: &SettingsRepo, id: &str) -> bool {
    if !contains(id) || tune_plugin_native::failure(id).is_some() {
        return false;
    }
    match settings.get(MIGRATION) {
        Err(_) => return false,
        // Fenêtre legacy (migration pas encore passée) : les greffons que la
        // migration embarque sont considérés actifs. L'égaliseur, facultatif,
        // n'y a pas droit : il retombe sur ses clés, comme après migration.
        Ok(marker) if marker.as_deref() != Some("complete") && installed_by_migration(id) => {
            return true;
        }
        _ => {}
    }
    installed(settings, id)
        && settings
            .get(&format!("plugin_{id}_enabled"))
            .is_ok_and(|v| v.as_deref() != Some("false"))
}
/// Une configuration d'égaliseur existe-t-elle déjà en base ? Au moins une clé
/// `zone_*_eq_profile` non vide, ou une liste `eq_presets` non vide. `false`
/// pour tout autre identifiant : seul l'égaliseur porte cette notion.
pub fn existing_configuration(settings: &SettingsRepo, id: &str) -> bool {
    if id != "equalizer" {
        return false;
    }
    settings.all().is_ok_and(|rows| {
        rows.iter().any(|(key, value)| {
            let zone_profile = key.starts_with("zone_")
                && key.ends_with("_eq_profile")
                && !value.trim().is_empty();
            let presets = key == "eq_presets"
                && serde_json::from_str::<Vec<serde_json::Value>>(value)
                    .is_ok_and(|presets| !presets.is_empty());
            zone_profile || presets
        })
    })
}
/// La proposition d'installation, DÉRIVÉE : elle s'éteint dès que l'utilisateur
/// a tranché, c'est-à-dire dès que `plugin_{id}_installed` existe (`true` par la
/// route d'installation, `false` par une désinstallation explicite). Aucune
/// écriture supplémentaire n'est nécessaire pour l'éteindre.
pub fn install_proposed(settings: &SettingsRepo, id: &str) -> bool {
    id == "equalizer"
        && flag(settings, id, "installed").is_none()
        && flag(settings, id, "install_proposed").as_deref() == Some("true")
}
/// Idempotent upgrade. Preserve explicit disabled/uninstalled flags and all
/// zone settings/presets. Write the marker last so an interrupted retry is safe.
pub fn migrate(settings: &SettingsRepo) -> Result<(), String> {
    migrate_for_account(settings, true)
}
/// Premium access for the three paid slots is separate; preserve explicit user
/// uninstall/disable choices. L'égaliseur n'est jamais installé ici : une
/// configuration existante ne vaut qu'une proposition, et un choix explicite
/// antérieur (`plugin_equalizer_installed` présent, vrai ou faux) la tait.
pub fn migrate_for_account(settings: &SettingsRepo, premium: bool) -> Result<(), String> {
    if settings.get(MIGRATION)?.as_deref() == Some("complete") {
        return Ok(());
    }
    for id in IDS {
        if !installed_by_migration(id) {
            propose_install_if_configured(settings, id)?;
            continue;
        }
        for suffix in ["installed", "enabled"] {
            let key = format!("plugin_{id}_{suffix}");
            if settings.get(&key)?.is_none() {
                settings.set(
                    &key,
                    if premium || !requires_premium(id) {
                        "true"
                    } else {
                        "false"
                    },
                )?;
            }
        }
    }
    settings.set(MIGRATION, "complete")
}
fn propose_install_if_configured(settings: &SettingsRepo, id: &str) -> Result<(), String> {
    if settings.get(&format!("plugin_{id}_installed"))?.is_some() {
        return Ok(());
    }
    if existing_configuration(settings, id) {
        settings.set(&format!("plugin_{id}_install_proposed"), "true")?;
    }
    Ok(())
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
    fn etat(s: &SettingsRepo) -> Vec<(String, String)> {
        let mut rows = s.all().unwrap();
        rows.sort();
        rows
    }
    /// Ce que fait la route `POST /plugins/{name}/install`.
    fn installer_par_la_route(s: &SettingsRepo, id: &str) {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
    #[test]
    fn egaliseur_base_neuve_ni_installe_ni_propose_ni_actif() {
        let s = settings();
        migrate_for_account(&s, false).unwrap();
        assert!(!installed(&s, "equalizer"));
        assert!(!install_proposed(&s, "equalizer"));
        assert!(!existing_configuration(&s, "equalizer"));
        assert!(
            !enabled(&s, "equalizer"),
            "EQ activé d'office sur base neuve"
        );
        assert!(s.get("plugin_equalizer_installed").unwrap().is_none());
        assert!(s.get("plugin_equalizer_enabled").unwrap().is_none());
        assert!(
            s.get("plugin_equalizer_install_proposed")
                .unwrap()
                .is_none()
        );
        assert_eq!(s.get(MIGRATION).unwrap().as_deref(), Some("complete"));
        let s = settings();
        migrate(&s).unwrap();
        assert!(!enabled(&s, "equalizer"), "EQ activé d'office pour Premium");
    }
    #[test]
    fn egaliseur_configuration_existante_proposee_puis_installee_par_la_route() {
        let s = settings();
        s.set("zone_1_eq_profile", "existing-free-profile").unwrap();
        migrate_for_account(&s, false).unwrap();
        assert!(!installed(&s, "equalizer"));
        assert!(existing_configuration(&s, "equalizer"));
        assert!(
            install_proposed(&s, "equalizer"),
            "profil existant non proposé"
        );
        assert!(!enabled(&s, "equalizer"), "proposer n'est pas activer");
        assert_eq!(
            s.get("zone_1_eq_profile").unwrap().as_deref(),
            Some("existing-free-profile")
        );
        installer_par_la_route(&s, "equalizer");
        assert!(enabled(&s, "equalizer"));
        assert!(
            !install_proposed(&s, "equalizer"),
            "la proposition doit s'éteindre à l'installation"
        );
        assert!(existing_configuration(&s, "equalizer"));
        assert_eq!(
            s.get("zone_1_eq_profile").unwrap().as_deref(),
            Some("existing-free-profile")
        );
    }
    #[test]
    fn egaliseur_presets_existants_declenchent_la_proposition() {
        let s = settings();
        s.set("eq_presets", r#"[{"id":"p1","name":"Salon","bands":[]}]"#)
            .unwrap();
        migrate(&s).unwrap();
        assert!(install_proposed(&s, "equalizer"));
        let s = settings();
        s.set("eq_presets", "[]").unwrap();
        s.set("zone_2_eq_profile", "   ").unwrap();
        migrate(&s).unwrap();
        assert!(!existing_configuration(&s, "equalizer"));
        assert!(!install_proposed(&s, "equalizer"), "liste vide proposée");
    }
    #[test]
    fn egaliseur_desinstalle_explicitement_nest_pas_repropose() {
        let s = settings();
        s.set("zone_1_eq_profile", "existing-free-profile").unwrap();
        s.set("plugin_equalizer_installed", "false").unwrap();
        migrate_for_account(&s, false).unwrap();
        assert!(!install_proposed(&s, "equalizer"));
        assert!(
            s.get("plugin_equalizer_install_proposed")
                .unwrap()
                .is_none()
        );
        assert!(!enabled(&s, "equalizer"));
        assert_eq!(
            s.get("zone_1_eq_profile").unwrap().as_deref(),
            Some("existing-free-profile")
        );
    }
    #[test]
    fn fenetre_legacy_ne_donne_jamais_l_egaliseur() {
        let s = settings();
        s.set("zone_1_eq_profile", "legacy-profile").unwrap();
        assert!(s.get(MIGRATION).unwrap().is_none());
        assert!(!enabled(&s, "equalizer"), "EQ actif avant migration");
        assert!(enabled(&s, "crossfeed"), "fenêtre legacy de JP perdue");
        // Une installation explicite vaut même sans le marqueur.
        installer_par_la_route(&s, "equalizer");
        assert!(enabled(&s, "equalizer"));
    }
    #[test]
    fn les_trois_autres_greffons_gardent_la_migration_d_origine() {
        for premium in [false, true] {
            let s = settings();
            migrate_for_account(&s, premium).unwrap();
            for id in ["crossfeed", "converter", "declick"] {
                assert_eq!(enabled(&s, id), premium, "{id} premium={premium}");
                assert_eq!(
                    s.get(&format!("plugin_{id}_installed")).unwrap().as_deref(),
                    Some(if premium { "true" } else { "false" }),
                    "{id} premium={premium}"
                );
                assert!(!install_proposed(&s, id));
                assert!(!existing_configuration(&s, id));
            }
        }
    }
    #[test]
    fn migration_idempotente_deux_appels_meme_etat() {
        for premium in [false, true] {
            let s = settings();
            s.set("zone_1_eq_profile", "legacy-profile").unwrap();
            s.set("zone_1_crossfeed", r#"{"enabled":true}"#).unwrap();
            migrate_for_account(&s, premium).unwrap();
            let premier = etat(&s);
            migrate_for_account(&s, premium).unwrap();
            assert_eq!(etat(&s), premier, "premium={premium}");
            assert!(install_proposed(&s, "equalizer"));
        }
    }
    #[test]
    fn premium_sdk_free_migration_keeps_eq_and_preserves_user_choices() {
        let s = settings();
        s.set("zone_1_eq_profile", "existing-free-profile").unwrap();
        migrate_for_account(&s, false).unwrap();
        // L'EQ n'est plus embarqué : il se garde (réglage intact) et se propose.
        assert!(!enabled(&s, "equalizer"));
        assert!(install_proposed(&s, "equalizer"));
        assert!(!enabled(&s, "converter"));
        assert!(!requires_premium("equalizer"));
        assert!(requires_premium("crossfeed"));
        assert_eq!(
            s.get("zone_1_eq_profile").unwrap().as_deref(),
            Some("existing-free-profile")
        );
        installer_par_la_route(&s, "equalizer");
        assert!(enabled(&s, "equalizer"));
        s.set("plugin_equalizer_enabled", "false").unwrap();
        migrate_for_account(&s, false).unwrap();
        assert!(
            !enabled(&s, "equalizer"),
            "migration reset an explicit user choice"
        );
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
