//! #9151 — le panneau « État des greffons » lisait `native_loaded` seul, et
//! un emplacement intégré sans paquet natif (le cas de tout le monde) y
//! paraissait « Not loaded » alors que son moteur embarqué tournait.
//!
//! Aucune bibliothèque native n'est chargée ici : `native_loaded` vaut
//! `false` pour chaque emplacement, comme chez le testeur du fil 2945.
use super::fiche_d_etat;
use tune_core::audio::premium_plugins;
use tune_core::db::settings_repo::SettingsRepo;

fn reglages() -> SettingsRepo {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let s = SettingsRepo::with_backend(std::sync::Arc::new(db));
    // Migration passée : l'état dépend des seules clés du greffon.
    s.set(premium_plugins::MIGRATION, "complete").unwrap();
    s
}

fn installer(s: &SettingsRepo, id: &str) {
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
}

/// ⭐ Le cas du fil : un intégré installé et actif, sans paquet natif, est
/// dit actif par son moteur embarqué — plus seulement « non chargé ».
#[test]
fn un_integre_actif_sans_paquet_natif_est_dit_actif_9151() {
    let s = reglages();
    installer(&s, "channel-remap");

    let fiche = fiche_d_etat(&s, "channel-remap", false);

    assert_eq!(fiche["native_loaded"], false, "aucun paquet natif ici");
    assert_eq!(
        fiche["engine"], "bundled",
        "#9151 : sans paquet natif, un intégré tourne par son moteur embarqué"
    );
    assert_eq!(
        fiche["active"], true,
        "#9151 : un intégré installé et activé doit être dit actif, pas « non chargé »"
    );
}

/// Contre-épreuve : le même emplacement, non installé ou désactivé, reste
/// inactif — le champ ne dit pas « actif » pour tout intégré.
#[test]
fn un_integre_non_installe_ou_desactive_reste_inactif_9151() {
    let s = reglages();
    assert_eq!(fiche_d_etat(&s, "channel-remap", false)["active"], false);

    installer(&s, "channel-remap");
    s.set("plugin_channel-remap_enabled", "false").unwrap();
    let fiche = fiche_d_etat(&s, "channel-remap", false);
    assert_eq!(fiche["active"], false, "désactivé : inactif");
    assert_eq!(fiche["engine"], "bundled");
}

/// Un greffon tiers demandé mais dont aucune bibliothèque n'est chargée
/// n'a pas de moteur : il n'est pas actif, et son moteur est `null`.
#[test]
fn un_tiers_sans_bibliotheque_chargee_n_est_pas_actif_9151() {
    let s = reglages();
    installer(&s, "greffon-tiers-9151");

    let fiche = fiche_d_etat(&s, "greffon-tiers-9151", true);

    assert_eq!(fiche["native_loaded"], false);
    assert!(
        fiche["engine"].is_null(),
        "tiers sans bibliothèque : pas de moteur"
    );
    assert_eq!(fiche["active"], false, "demandé mais non chargé : inactif");
    assert_eq!(fiche["third_party"], true);
}
