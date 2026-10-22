//! #6256 — la désinstallation COMPLÈTE d'un greffon audio natif tiers :
//! déchargé, toutes ses versions retirées du disque, ses réglages effacés ou
//! gardés au choix, et rien de supprimé hors de son dossier.
//!
//! Aucune bibliothèque n'est chargée ici : le déchargement d'une bibliothèque
//! réelle est éprouvé par le banc `#[ignore]` de
//! `tune-core/src/orchestrator/greffon_natif_tiers_tests.rs`.
use super::desinstaller_completement;
use std::path::Path;
use tune_core::db::settings_repo::SettingsRepo;

fn reglages() -> SettingsRepo {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    SettingsRepo::with_backend(std::sync::Arc::new(db))
}

/// Un greffon installé comme `package::install` le range : une version, son
/// état actif, et des données à lui dans son dossier.
fn installer(racine: &Path, id: &str) {
    let version = racine.join(id).join("versions").join("abc123");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::write(version.join("libgreffon.so"), b"binaire").unwrap();
    std::fs::write(version.join("bundle.tuneplugin"), b"archive").unwrap();
    std::fs::write(
        racine.join(id).join("active.json"),
        br#"{"current":"abc123"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(racine.join(id).join("donnees")).unwrap();
    std::fs::write(racine.join(id).join("donnees").join("etat.bin"), b"x").unwrap();
}

fn regler(s: &SettingsRepo, id: &str) {
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    s.set(&format!("plugin_{id}_version"), "1.0.0").unwrap();
    s.set(&format!("native_plugin_{id}_profiles"), "[]")
        .unwrap();
    s.set(&format!("zone_3_native_plugin_{id}"), r#"{"enabled":true}"#)
        .unwrap();
}

/// ⭐ Le cas de l'issue : binaires, versions et données retirés, réglages et
/// profils effacés, échec de chargement oublié — et le greffon voisin intact.
#[test]
fn la_desinstallation_complete_retire_les_fichiers_et_les_reglages_6256() {
    let racine = tune_core::test_scratch::scratch_dir("desinstallation-6256-complete");
    let id = "greffon-essai-6256-a";
    installer(&racine, id);
    installer(&racine, "voisin-6256");
    let s = reglages();
    regler(&s, id);
    regler(&s, "voisin-6256");
    tune_plugin_native::record_failure(id, "échec d'essai".into());

    let bilan = desinstaller_completement(&racine, &s, id, false).unwrap();

    assert!(bilan.fichiers_retires);
    assert!(
        !racine.join(id).exists(),
        "le dossier du greffon (versions, binaire, données) doit avoir disparu"
    );
    assert!(
        racine
            .join("voisin-6256")
            .join("versions")
            .join("abc123")
            .join("libgreffon.so")
            .is_file(),
        "le greffon voisin ne doit pas être touché"
    );
    assert_eq!(
        bilan.reglages_effaces.len(),
        5,
        "{:?}",
        bilan.reglages_effaces
    );
    for (cle, _) in s.all().unwrap() {
        assert!(
            !cle.contains(id),
            "réglage du greffon désinstallé resté : {cle}"
        );
    }
    assert_eq!(
        s.get("zone_3_native_plugin_voisin-6256")
            .unwrap()
            .as_deref(),
        Some(r#"{"enabled":true}"#),
        "le réglage du voisin ne doit pas être effacé"
    );
    assert!(
        tune_plugin_native::failure(id).is_none(),
        "l'échec de chargement d'un greffon désinstallé doit être oublié"
    );
    assert!(tune_plugin_native::provider(id).is_none());
    assert!(!crate::native_audio::is_third_party(id));
}

/// Au choix de l'utilisateur : les réglages de zone et les profils restent
/// pour une réinstallation ; les fichiers partent quand même, et le greffon
/// n'est plus demandé.
#[test]
fn garder_les_reglages_garde_zones_et_profils_mais_retire_les_fichiers_6256() {
    let racine = tune_core::test_scratch::scratch_dir("desinstallation-6256-garder");
    let id = "greffon-essai-6256-b";
    installer(&racine, id);
    let s = reglages();
    regler(&s, id);

    let bilan = desinstaller_completement(&racine, &s, id, true).unwrap();

    assert!(bilan.fichiers_retires && !racine.join(id).exists());
    assert!(bilan.reglages_effaces.is_empty());
    assert!(
        s.get(&format!("zone_3_native_plugin_{id}"))
            .unwrap()
            .is_some()
    );
    assert!(
        s.get(&format!("native_plugin_{id}_profiles"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        s.get(&format!("plugin_{id}_installed")).unwrap().as_deref(),
        Some("false")
    );
    assert!(s.get(&format!("plugin_{id}_version")).unwrap().is_none());
    assert!(!tune_core::audio::natifs_tiers::demande(&s, id));
}

/// La prudence : un dossier de greffon qui est un LIEN vers ailleurs n'est pas
/// suivi. Rien n'est supprimé, ni la cible, ni les réglages.
#[cfg(unix)]
#[test]
fn un_dossier_de_greffon_qui_pointe_ailleurs_n_est_pas_suivi_6256() {
    let racine = tune_core::test_scratch::scratch_dir("desinstallation-6256-lien");
    let ailleurs = tune_core::test_scratch::scratch_dir("desinstallation-6256-ailleurs");
    std::fs::write(ailleurs.join("precieux.txt"), b"a garder").unwrap();
    let id = "greffon-essai-6256-c";
    std::os::unix::fs::symlink(ailleurs.path(), racine.join(id)).unwrap();
    let s = reglages();
    regler(&s, id);

    assert!(desinstaller_completement(&racine, &s, id, false).is_err());
    assert!(
        ailleurs.join("precieux.txt").is_file(),
        "hors du dossier : rien ne part"
    );
    assert!(
        s.get(&format!("zone_3_native_plugin_{id}"))
            .unwrap()
            .is_some(),
        "un échec sur les fichiers n'efface aucun réglage"
    );
}

/// Les quatre emplacements intégrés ne passent jamais par ici.
#[test]
fn un_emplacement_integre_n_est_pas_purge_6256() {
    let racine = tune_core::test_scratch::scratch_dir("desinstallation-6256-integre");
    installer(&racine, "crossfeed");
    let s = reglages();
    assert!(desinstaller_completement(&racine, &s, "crossfeed", false).is_err());
    assert!(racine.join("crossfeed").join("active.json").is_file());
    assert!(s.get("plugin_crossfeed_installed").unwrap().is_none());
}
